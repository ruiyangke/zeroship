# The CDC service

This document specifies the CDC relay service. The decision to build it is
recorded in `docs/proposals/2026-08-26-runtime-db-binding-00-index.md:87-92`;
this page does not re-argue it.

**Working name:** `zeroship-cdc`. One binary, one process, executes no creator
code.

**Implementation status at `df8cf8472`: design only.** `zeroship-cdc`, its wire
crate, Datastore, Database and Grant, the CDC control surface, process credentials
and `__zeroship_admin.database_heads` do not exist. Section 3.7 makes them
prerequisites, not descriptions of the tree.

---

## 0. How to read the citations in this document

Every `file:line` below was opened in this worktree
(`/home/ruiyang/Projects/appbase/.worktrees/dbbind-impl`), on the working tree as
it stands. The migration service crate is `crates/zeroship-migrate-server/`
(renamed from `zeroship-migrated` in `8f69c7e53`).

**This proposal sets PostgreSQL major 18 as the platform target for CDC; 18.4 is
the verification and deployment release.** That is a decision made here, not an
inference from the generated migration-feature matrix at
`docs/support-matrix.md:11`. The existing platform is not aligned with it: dev
compose pins `postgres:16` (`deploy/compose/docker-compose.yml:73`), the
`compio-postgres` cross-version runbook calls 16 the driver floor
(`docs/runbooks/compio-postgres-cross-version-check.md:205`), and CI exercises
16 and 17. The 18.4 evidence is local measurement, not a target gate. A driver
floor is not a production target. Before the relay can ship, the platform deploy
image and every CDC acceptance image move to 18.4.
Relay startup and Datastore provisioning read `server_version_num` and refuse a
major outside `[180000, 190000)`. There is no 16/17 branch, signature probe, or
compatibility fallback. The accepted cost is that operators must upgrade and
restart PostgreSQL before enabling CDC.

PostgreSQL behaviour claims marked **MEASURED** came from isolated throwaway
servers with `wal_level=logical`. The version ledger below is part of each
claim; "measured" without a server version is not target evidence. Every
PostgreSQL behaviour claim is version-sensitive in the literal sense that a
later major may change it. The last column distinguishes target evidence from a
claim that must be repeated on 18.4 before implementation can rely on it.

| claim | measured server | version sensitivity and decision |
| --- | --- | --- |
| Publication column lists remove excluded names and values; a new column defaults out (3.2-3.3) | both properties on 16.15; filtering independently repeated on 18.4 (`docs/proposals/2026-08-28-app-database-decoupling.md:903-918`), but not the new-column arm | Version-sensitive. Filtering has target evidence; 10.3's 18.4 new-column fixture must establish the default-out arm before implementation relies on it |
| Missing replica identity breaks UPDATE/DELETE with `42P10`; `REPLICA IDENTITY FULL` is incompatible; conflicting lists fail decode (3.5) | 18.4 | Version-sensitive. This is target evidence and remains a live-server acceptance test |
| Publications are Datastore-scoped; slot namespace/budget is cluster-scoped; one logical slot decodes one Datastore (3.6, 6.1) | all three properties on 18.4 (`docs/architecture/data-system.md:394-413`); this proposal has no retained PostgreSQL-16 publication-locality transcript | Version-sensitive. No evidence supports a 16/18 split; the old one-slot-per-cluster claim was an inference from the wrong scope, not a PostgreSQL-16 measurement |
| A publication member can be changed atomically with DROP/ADD; published columns are catalog dependencies; `attnames` follows renames (3.6) | 18.4 | Version-sensitive. This is target evidence |
| Per-change LSNs can go backwards across commit order and collapse under `heap_multi_insert` (6.3) | 18.4 | Version-sensitive. This is target evidence |
| Promotion changes timeline while crash recovery does not (6.4) | 17.11 | Version-sensitive and not target evidence. The 18.4 failover gate must repeat it |
| Stock slot/WAL GUC defaults and slot invalidation (6.1, 7.2) | `max_replication_slots` and `max_slot_wal_keep_size` on 18.4 (`docs/architecture/data-system.md:401-426`); `max_wal_senders` in 7.2's 18.4 `pg_settings` output; the original invalidation transcript did not record its server | Configuration- and version-sensitive. Read live values; repeat all defaults and invalidation on the 18.4 acceptance image |
| `logical_decoding_work_mem` has a 64 MiB boot value (7.2, 13) | 18.4, in 7.2's `pg_settings` output | Configuration- and version-sensitive. It is a memory-sizing input, not a guaranteed live value; startup records the live setting and the load gate exercises spill |
| `safe_wal_size` becomes numeric only with a finite WAL cap (7.6) | 18.4 | Configuration- and version-sensitive. This is target evidence, never a portable default |
| Transactional logical messages are ordered with DDL and later changes (3.6, 8) | 16.15, 17.11 and the 18.4 transcript at 3.6 | Version-sensitive. Ordering has target evidence and remains an acceptance test |
| Logical messages are omitted unless `messages=true` (8) | 16.15 and 17.11 | Version-sensitive and not target evidence. The 18.4 acceptance gate must repeat the negative; PostgreSQL 17 also changed the function identities retained by 18 |
| `pg_control_checkpoint().timeline_id` exposes the current timeline to SQL (6.4) | 18.4 | Version- and privilege-sensitive target evidence. It is diagnostic only; `IDENTIFY_SYSTEM` on the streaming connection remains authoritative |
| Replay order was byte-identical in the bounded probe (12.6, 13) | 17.11 | Version-sensitive and not target evidence. It remains a bounded assumption until repeated on 18.4 |
| A transactional publication swap is atomic to decode but prospective, not retroactive (3.6, 13) | 17.11 | Version-sensitive and not target evidence. The 18.4 bracket gate repeats it |

One version trap admits two simultaneously true readings. PostgreSQL 16 exposes
text and bytea three-argument `pg_logical_emit_message` identities. PostgreSQL
17 and 18 expose text and bytea four-argument identities with a defaulted
`flush` argument. A three-argument call can therefore look portable while an ACL
statement naming either 16 identity matches nothing on 18. Section 8 chooses the
18 identities explicitly. This is exactly the version split that can pass in a
16-based test and fail in an 18 deployment.

Two things this document deliberately does not carry: any latency figure (section
11 specifies how to obtain one) and any re-derivation of the decode multiplier,
which the index records as structural and already verified in the PostgreSQL
sources.

---

## 1. Decision summary

| question | answer | section |
| --- | --- | --- |
| Where is the wire projection applied? | In the **publication column list**, as DDL, by `zeroship-migrate-server`. PostgreSQL never puts the excluded bytes or the excluded names on the wire - **from the swap forward.** Measured: changes already in the WAL when a column is newly excluded still carry it, so the projection is prospective, not retroactive (13) | 3 |
| What covers `changed_columns`? | The same mechanism. A column absent from the publication list is absent from the pgoutput `Relation` message, which is the only thing `changed_columns` is built from | 3.2 |
| How many slots, streams and publications? | One slot, one pgoutput stream and one shared publication **per Datastore**, all owned by one relay leader per physical cluster. Creating a Database edits the Datastore publication live; creating a Datastore starts another stream | 3.6, 6.1 |
| When is membership reconciled? | **Bracketing** the migration DDL - shrink before, widen after - because a published column is a catalog dependency and `DROP COLUMN` of one fails `2BP01` | 3.6 |
| Where does the column set come from? | The same policy-resolved fold that produces the DDL. The migration service holds the IR, not the descriptor, and must not re-derive `valueColumn` by string formatting | 3.7 |
| Which code moves? | The reusable decoder and exact-slot algorithms are extracted and rewritten for relay-owned types; the plugin-coupled parts of `wal_consumer.rs` and `replication.rs` do not move literally. `slot_reaper.rs`, `replication_ops.rs`, `change_stream_pg.rs` and the **whole** local-emit path are **deleted**; `broker.rs`, `read_set.rs`, `cdc_lifecycle.rs` stay | 4 |
| Is `zeroship-stream` the carrier? | **No**, and the reasons are in its own source, not in taste | 5.1 |
| What is the carrier? | A volatile relay-owned bounded per-app ring plus an immutable-registration long-lived response over `ntex` (server) and `cyper` (client), authenticated by a service assertion | 5.2 |
| What happens to a slow worker? | It is shed, never awaited. Ring overrun sends that connection an unsequenced `Resync`; a wedged connection is closed. No credit scheme, because credit would let a consumer stall slot confirmation | 5.4 |
| How big is the ring? | Byte depth, frame cap and time depth, per app, plus a cluster cap that evicts from the largest ring | 5.5 |
| What is the crash-delivery contract? | Same-term **worker transport** reconnects replay while the per-app cursor remains in the ring; a same-term Datastore replication reconnect instead resets as `DatastoreReconnect`. Every higher leader term returns `Registered::Reset(RelayFailover)` before app data. Incremental events may be lost across relay failure; live queries recover by refetch and raw subscribers must handle the reset | 6.3-6.4 |
| What if a change cannot be represented? | A `Gap` frame for that row, not a `Resync` for the app. Plus a `Truncate` frame and a three-way value encoding that distinguishes NULL from unavailable | 5.6 |
| Leader election | One session advisory lock per `cluster_id` in the designated coordination database. A control-recorded predecessor-worker fence blocks every new-term topology ACK until each old-term worker drains or a supervisor proves that exact process dead and its credential revoked. The leader owns one exact slot/stream per Datastore | 6 |
| Duplicate-suppression key | `(commit_lsn, change_index)`, lexicographic. The per-change LSN is unusable: measured to go backwards between transactions and to collapse under `heap_multi_insert` | 6.3 |
| What invalidates a watermark? | A `systemid`, `timeline`, Datastore replication reconnect, Database rebind or leader-term change clears the numeric dedup watermark. Duplicate raw events are accepted rather than risking suppression after a rewind or failover | 6.4 |
| Failure behaviour | `max_slot_wal_keep_size` bounds WAL; same-term slot invalidation resets apps before the slot is dropped and recreated, then restores from a complete durable-head snapshot. A process restart is observed instead as `RelayFailover` | 7 |
| Blast radius of one tenant | Bounded by construction: a total per-app step, no `catch_unwind`, and a degraded app is quarantined without stopping the slot | 7.5 |
| What does the relay report? | Relay-side, per app and per Datastore. `confirmed_flush_lsn` and `safe_wal_size` are structurally blind to a worker-delivery outage under this gap-signaled contract | 7.6 |
| Schema-change signal | A PostgreSQL-18 `pg_logical_emit_message` inside the **widen** transaction, after revoking both built-in overloads from `PUBLIC`. WAL ordering solves ordering; the ACL makes the marker authoritative | 8 |

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
whole process, not one per app). The relay's separate streaming login takes only
`REPLICATION`. It does not receive `BYPASSRLS` and never issues creator-table
SQL. `zeroship_worker` drops both `REPLICATION` and `BYPASSRLS` in the same
migration that provisions the relay's streaming role.

`BYPASSRLS` is a separate grant from `REPLICATION` and is not required to consume
a slot. Section 13 records the completed consumer audit: no production RLS policy
depends on the worker attribute, while the worker posture check currently
requires both attributes solely for logical decoding. When decoding moves, that
check must require both to be absent.

### 2.2 L12b: an abandoned slot

Slot names are per `(app, worker)`:
`crates/zeroship-plugin-db/src/replication.rs:114-121` composes
`__zs_slot_<sha14(app)>__<sha10(worker)>`.
`crates/zeroship-plugin-db/src/slot_reaper.rs` exists (593 lines) to sweep the
ones nobody owns any more, on a one-hour inactivity threshold (`:28`) with a
two-lock worker lease (`:116-125`, `:182-198`) and a fleet-leader election
(`:171-180`). With O(Datastores) service-owned slots and exactly one owner per
`cluster_id` there is no per-worker slot to abandon. The file is deleted, not
ported.

### 2.3 The mask-only boundary, stated precisely

The shipped downstream serializers currently keep raw names and values out of
creator responses. That defense is real, but it is one process boundary too
late for the relay:

- `crates/zeroship-plugin-db/src/wal_consumer.rs` contains zero occurrences of
  `mask`, `wrap_row_on_read` or `apply_mask`. `tuple_to_map` (`:662-681`) zips
  every physical column sent by pgoutput into `new_tuple`, while
  `changed_columns` is every cached Relation column (`:629-633`).
- `new_tuple` is still consumed inside the worker for subscription predicate
  evaluation (`crates/zeroship-plugin-db/src/broker.rs:281-299`). Thus the raw
  name and value enter pgoutput, the relay-to-worker path being extracted, and
  the broker even though they need not be returned to creator code.
- The current creator serializers are safe: `message_to_json` filters platform
  names through `creator_visible_columns` and emits no row values
  (`broker.rs:927-978`), and `ws_frame` applies the same filter and also emits no
  values (`broker.rs:1007-1027`). The obsolete value-carrying
  `ws_frame_for_change` has already been deleted.
- The Postgres local-emit path is also upstream of masking:
  `exec_mutation_with_emit` calls `emit_for_rows` at `exec.rs:438` before the
  caller applies the read pipeline at
  `crates/zeroship-plugin-db/src/crud/mod.rs:812-824`. Section 4 deletes that path
  rather than treating its downstream name filter as a security boundary.

The publication whitelist therefore does not repair a current creator-response
leak. It moves the security boundary to PostgreSQL so excluded names and bytes
never enter pgoutput or either service process. The accepted cost is that every
schema migration must maintain that whitelist correctly.

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
    { storage.valueColumn(field)
      : field in declared_fields(collection)
        AND wire_exposure(field) IN { Plaintext, Masked } }
```

`storage.valueColumn` is the descriptor's field-to-physical-column mapping, the
same block `value_column_for_field` reads on the query side
(`crates/zeroship-schema/src/query.rs:3436-3455`). The storage flip has shipped:
the logical field column holds the creator-visible value, including the mask,
and `storage.rawColumn` names the `__zs_raw__<field>` column that holds plaintext
or ciphertext (`crates/zeroship-migrate-core/src/render/gen_types.rs:289-325`;
`crates/zeroship-plugin-db/src/crud/mask_pass.rs:180-221`). `rawColumn` is never
in the wire set because the set is built from `valueColumn` and nothing else.

`wire_exposure` is a closed renderer result, not a guess from names:
unclassified storage is `Plaintext`; a field with a physical `rawColumn` has a
safe `Masked` `valueColumn`; and a field with non-public classification but no
separate safe representation is `ExcludedProtected`. The last case includes
the supported explicit `.mask({ kind: "none", classification: ... })` shape:
`raw_column_for_field` deliberately returns none
(`crates/zeroship-migrate-backend/src/schema.rs:583-609`), so publishing its
only column cannot be proven to withhold classified plaintext. This design
excludes it. If an excluded protected field is part of replica identity, the
migration is refused rather than unioning it back in. The accepted cost is that
updates visible only through such a field cannot drive incremental
subscriptions; reads and reset refetches still see the authorized value.

The set is computed by the migration service, which has just folded the migration
and therefore holds the declared field set with its mask and encryption flags. It
is not derived from a string suffix at any point. **Where exactly that fold
happens, and why the service cannot read a descriptor it never receives, is 3.7.**

### 3.2 MEASURED: the column list removes the name as well as the value

This is the load-bearing claim, and it is the reason one mechanism satisfies both
halves of the inherited requirement. The PostgreSQL 18.4 filtering measurement
is recorded in
`docs/proposals/2026-08-28-app-database-decoupling.md:903-918`. The target
fixture is:

```sql
CREATE TABLE t (id int primary key, ssn text, __zs_raw__ssn text);
CREATE PUBLICATION p1 FOR TABLE t (id, ssn);
SELECT pg_create_logical_replication_slot('s1','pgoutput');
INSERT INTO t VALUES (1,'***-**-6789','123-45-6789');
SELECT encode(data,'escape')
  FROM pg_logical_slot_peek_binary_changes('s1',NULL,NULL,
       'proto_version','1','publication_names','p1');
```

Decoded, the `Relation` names exactly `id` and `ssn`, and the `Insert` carries
`1` and `***-**-6789`. Neither the name `__zs_raw__ssn` nor the bytes
`123-45-6789` occur anywhere in the stream. Section 10.2 repeats this through
the real migration and plugin write paths rather than trusting hand-written DDL.

`changed_columns` is built from the relation cache, which is built from the
`Relation` message (`wal_consumer.rs:511-526` populates it, `:629-633` maps it).
A name the server never sends cannot be put in `changed_columns` by any code
path, present or future.

### 3.3 MEASURED: a new column defaults OUT

```sql
ALTER TABLE t ADD COLUMN newcol text;
INSERT INTO t VALUES (2,'***-**-9999','999-99-9999','brand new');
```

The next `R` frame is byte-identical to the one above (`id`, `ssn`), and
the `I` frame carries two values, `2` and `***-**-9999`. Neither the new column
name, nor its value, nor the new plaintext appears.

This is the property that a downstream raw-column stripper does not have and
cannot have. A name-based blacklist protects only names it anticipates. A
publication column list is a whitelist over the columns the migration service
decided to publish, and everything not on it is absent by default, including
columns that do not exist yet.

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

`changed_columns` is not constructed independently. Both producers derive it
only from `ProjectedTuple::visible_keys()`. The target `ChangeEvent` deletes
`old_tuple`: old tuples do not cross the relay wire or the broker boundary.
For an identity-changing UPDATE, `pk` is the pre-change replica-identity vector;
the new identity remains in the positional values. Otherwise `pk` is the current
identity for INSERT/UPDATE and the deleted identity for DELETE. The relay and
SQLite producer discard every other old value before publishing.
Section 5.3's bounded delivered-pk set replaces before-image predicate
evaluation. This closes the name vector as well as the value vector; projecting
only `new_tuple` while retaining SQLite's current key collection at
`sqlite/cdc.rs:622-623` would not.

The newtype does not create the guarantee. Its job is to make every producer
**name** which guarantee it is relying on, so that a third producer cannot be
written that relies on none. That is the same move
`docs/reviews/2026-08-28-flip-write-path.md:368` makes with `RawRows` on the read
path, and on the relay arm the constructor is close to a no-op.

### 3.5 What the column list costs, and it is not free

Three consequences measured on PostgreSQL 18.4. All three are real hazards and
the design carries them explicitly.

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
`\set VERBOSITY verbose`, against a table published as `(ssn)` only:

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
`(id, ssn)` makes every `UPDATE` and `DELETE` fail with the same error,
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

Under 3.6 each creator table belongs to exactly one relay-owned publication in
its Datastore, so this cannot arise from the intended shape. It can arise from
an operator or a test creating a second publication over a creator table, so
the relay treats the decode error as fatal for that Datastore stream and names
the table rather than reconnecting into it.

### 3.6 One shared publication per Datastore, and a locked DDL bracket

Two things force this subsection, and they interact. Take them in order.

**(i) There is exactly one shared publication per Datastore.** A Datastore is
one physical PostgreSQL database. Publications are database-local:
`SELECT relisshared FROM pg_class WHERE
oid = 'pg_catalog.pg_publication'::regclass` returns false, and the same
publication name may coexist in two Datastores of one physical cluster.
`relisshared` is a property of the catalog relation in `pg_class`, not a column
of `pg_publication`. A logical slot also decodes only the database in which it
was created. These are PostgreSQL 18.4 measurements recorded at
`docs/architecture/data-system.md:394-419`.

Consequently each Datastore has one slot, one pgoutput stream and one shared
publication. The one relay leader for the physical cluster owns all of those
Datastore streams. The stock `max_replication_slots = 10` is a hard capacity
ceiling: at one slot per Datastore the cluster admits at most ten Datastores,
fewer if any other slot exists. `max_wal_senders` is independently ten by
default and can be the active-stream ceiling first. Both are
`context = postmaster` settings; raising either restarts the shared cluster.
Datastore provisioning therefore reserves both capacities before declaring the
Datastore CDC-ready and fails closed when either reservation is unavailable.

**A surviving authority conflict is called out, not propagated.**
`docs/architecture/data-system.md:394-442` has the corrected measurements and
one-slot-per-Datastore conclusion. Its later text at `:450-461` still says
creating a Database creates a publication and reasons about adding that
publication to one shared slot. Those statements cannot survive the corrected
decode scope. Likewise,
`docs/proposals/2026-08-28-app-database-decoupling.md:922-947` still chooses
one slot per (Datastore, worker) and one publication per Database. This proposal
chooses the implementable relay shape: creating a Database edits its
Datastore's existing shared publication; creating a Datastore creates the
publication, slot and stream. The two other documents need a separate
correction before implementation begins.
The routing table in
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md:25` also still calls
this "one cluster publication"; that summary must change with them.

The same proposal also says the schema epoch never enters WAL and any logical
marker is forgeable
(`docs/proposals/2026-08-28-app-database-decoupling.md:966-971`). Section 8's
PostgreSQL-18 ACL and migration-service-only emitter deliberately supersede both
statements. That document must be corrected with the publication claims; leaving
both readings in the design set would make the bracket impossible to implement
consistently.

The shared objects are keyed by the typed `DatastoreId`, never by `app_id`:

```text
datastore_publication_name(datastore_id) = "__zs_pub_" + hex(sha256(id)[0..14])
datastore_slot_name(datastore_id)        = "__zs_slot_" + hex(sha256(id)[0..14])
```

The current `publication_name(app_id)` hashes the app id
(`crates/zeroship-core/src/replication_names.rs:17-32`), and
`reconcile_app_publication` calls it with `app_id`
(`crates/zeroship-migrate-server/src/publication.rs:59-66`). That API is
replaced, not retained or aliased. The implementation adds the two Datastore
functions above and updates every caller in the same change.

`publication_names` is fixed at `START_REPLICATION`: the driver quotes and
interpolates the list once
(`libs/compio-postgres/src/replication.rs:727-743`; the option type is
`:844-852`). The fixed list is not a conflict because each Datastore stream
names exactly its Datastore's one publication for its whole life. Creating a
Database adds its schema's table memberships to the existing publication
without restarting the stream. Creating a new Datastore provisions its empty
publication and exact slot, then starts an additional stream before any
Database may be placed there. The accepted cost is one walsender, one
replication connection and one slot from the cluster budget for every Datastore.

The same publication always contains the reserved
`__zeroship_cdc.heartbeat (id, nonce)` member used by 6.3. Per-Database
reconciliation treats that member as an invariant system entry: it neither
returns it in a Database diff nor drops it when the last creator table leaves.
Using a second heartbeat publication is rejected because it would create a
second fixed start-time name for no isolation benefit.

**This is one narrow, deliberate supersession of the current architecture
rule.** `docs/architecture/data-system.md:389-392` says publication membership
excludes the whole `__zeroship_` namespace. That rule cannot coexist with a
decoded idle-progress heartbeat. It is replaced by: no reserved relation is a
member except the exact `__zeroship_cdc.heartbeat` table with the exact
`(id, nonce)` projection. Provisioning records its relation OID, and the decoder
requires that OID, qualified name and column shape before treating a change as
a heartbeat. Any other reserved member or shape makes the Datastore
unavailable. The frame is consumed by the relay and never enters an app ring or
worker response. The accepted cost is one platform relation in pgoutput and a
hard-coded exception that the architecture document must adopt; the benefit is
idle confirmation based on decoded WAL rather than an unsafe keepalive
position.

MEASURED, PostgreSQL 18.4. One publication `zs_cdc` in one Datastore over two
Database schemas - `app_alpha.notes (id, ssn)` and
`app_beta.items (id, label)`, the state
left by the bracket transcript in (iii) below - with a slot already streaming:

```sql
SELECT pg_create_logical_replication_slot('sc','pgoutput');
INSERT INTO app_alpha.notes VALUES (1,'x','***1');
CREATE TABLE app_gamma_items (id int primary key, label text, secret text);
BEGIN;
  ALTER PUBLICATION zs_cdc ADD TABLE app_gamma_items (id, label);
  SELECT pg_logical_emit_message(
    true, 'zs.database_epoch', 'dbs_gamma:1'::text, false
  );
COMMIT;
INSERT INTO app_gamma_items VALUES (9,'live','top-secret');
INSERT INTO app_alpha.notes VALUES (2,'y','***2');
```

peeking that slot with `'messages','true'`:

```
 0/1805D18 | 783 | B
 0/1805D18 | 783 | R ... app_alpha notes ... id, ssn
 0/1805D18 | 783 | I ... 1, ***1
 0/1805E30 | 783 | C
 0/180CB98 | 785 | B
 0/180CB98 | 785 | M zs.database_epoch ... dbs_gamma:1
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
buys nothing across tenants. With the Datastore publication that key is constant
inside one database, so it serialises membership reconciliation within that
Datastore - and nowhere else, because PostgreSQL advisory locks are
database-scoped. A serialised full replace is still a full replace. Last writer
wins cleanly instead of racily. Keep the lock as the Datastore publication
mutex, but replace `SET TABLE` with per-member `DROP TABLE` plus `ADD TABLE`.
The accepted cost is serial migration publication work for Databases sharing
one Datastore, not for the whole physical cluster.

The publication xact lock is not the marker-loss fence. The target adds one fixed
Datastore-local session advisory read/write fence. Every ordinary head writer
takes shared before its transaction or publication mutex and revalidates
`cdc_state == Ready` within three seconds. Failure unlocks and retries after a
fence-free wait; ambiguous unlock closes the session. Commit or abort releases
shared before any remote wait.

A destructive reset holds exclusive through heartbeat, `RestoreReady` and
durable `Ready`. In `Quiesce`, only its exact unsealed classification token may
authorize that migration's T1, repair, journal-only or T4 head transaction;
finalize seals it before `Restore`, and durable state refuses unrelated writes
after relay death. Both arms use fixed
`(CDC_DATASTORE_RESET_NAMESPACE, 0)` through `pg_advisory_lock_shared` or
`pg_advisory_lock` in the target database, never a tenant hash.

**(iii) A published column is a catalog dependency.** MEASURED, 18.4, on a table
published as `(id, b, ssn)`:

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

`reconcile_app_publication` currently runs after `apply_sealed` and after its
project lock has been released
(`crates/zeroship-migrate-server/src/apply.rs:445-462`). A migration that drops
an ordinary published column therefore aborts at the DDL before reconciliation
can narrow the list. Removing `body` from the collection is the acceptance
case. Removing `.mask()` is not: after the shipped storage flip
`storage.valueColumn` remains the logical field name, and the IR has no
remove-mask operation. The old proposal's mask-removal example was inverted.

**The specification: shrink before, DDL, widen after.** Three steps, and the
first and third are their own transactions on the same pinned migration
connection while the existing Database lock remains held. The shrink and widen
transactions also take the publication mutex; the head-advancing widen/T4 takes
the shared reset fence unless it carries the exact classification token above.

1. **Shrink.** For every table of this Database currently in the publication, set its
   column list to `old_wire INTERSECT new_wire`. A table leaving the declared set
   entirely is dropped from the publication. The published set never grows here.
2. **The DDL**, through the existing per-file engine path with
   `LockMode::AlreadyHeld`.
3. **Widen.** For every table in the Database's new declared set, set its column list
   to `new_wire`; add tables that were not members. The epoch marker of section 8
   is emitted in **this** transaction.

**A field becoming protected takes a Datastore reset barrier, not the ordinary
bracket alone.** During its one ordered fold, the host compares field exposure
at the exact net-applied migration set with the projection that actually
committed. Any existing field changing from wire-plaintext to `Masked`,
`ExcludedProtected` or another non-plaintext representation enters
`newly_protected_fields`. Old WAL may still contain plaintext under the same
physical field even when its logical table or field name changed, so the
publication intersection cannot remove it.

The fold assigns an internal `ProjectionFieldKey` to every checkpoint field and
carries that key through ordered `renameTable` and `renameColumn` operations;
a create receives a new key and a drop retires one. The fold also carries every
checkpoint wire-plaintext name through the ordered table and column rename map,
even after its key is retired. A reset is required when either (a) one
provenance key changes from plaintext to protected, or (b) a final protected
qualified name collides with one of those mapped historical plaintext names.
The second rule deliberately catches drop-and-create reuse, where pgoutput has
no lineage identifier. `newly_protected_fields` reports the final qualified
name in both cases. This includes a rename followed by classification in the
same bundle and adding encryption with explicit mask kind `none`. False-positive
Datastore resets on ambiguous rename/drop chains are accepted; a missed
collision can strand pre-DDL plaintext in retained WAL.

Before shrink, the migration service calls
`PrepareClassifiedMigration(database_id, expected_epoch, apply_attempt_id,
apply_fingerprint)`. `apply_attempt_id` is a caller-stable typed UUIDv7 key for
one request; resubmission uses a new key. The fingerprint hashes canonical
ordered identities, checksums and resolved policy, never journal progress.
Control serializes per Datastore, persists same-kind `Quiesce` generation G and
publishes it. The relay takes exclusive, cancels, closes and joins the decoder
before any ring or ACK; cancellation fences late ingress. Per ring it appends
`AppReset(ClassificationChanged)` at `ring_next_seq`, increments once, removes
older frames and sets `tail_seq` to the reset sequence; a term never reuses a
sequence. It clears egress and gives each invalidated connection staged primes
plus local `Resync` at the post-fence cursor, or closes it if control cannot queue.
It then drops the exact slot and sends exact-generation `Quiesced` only after
`pg_replication_slots` proves absence.

Control returns an opaque token and `MigrationApplyId` bound to Datastore,
generation, Database, expected epoch, attempt and fingerprint. Exact
attempt/fingerprint replay returns the same token, id and result; changed content
conflicts, and a new attempt after `Ready` may start a new generation. An
unavailable participant refuses before DDL. The relay retains exclusive; during
`Quiesce`, only that Database and apply's unsealed token authorize its head
transactions, while durable state refuses every unrelated write.

The ordinary shrink, DDL and widen then run. After success or a converged engine
failure, the migration service sends one idempotent typed request:

```text
POST /internal/v1/cdc/datastores/{datastore_id}/classification-resets/{reset_generation}/finalize
Authorization: Bearer <migration-service assertion>
Content-Type: application/json

{
  "database_id": DatabaseId,
  "migration_apply_id": MigrationApplyId,
  "final_head_epoch": DatabaseEpoch,
  "last_rotation": MigrationApplyId,
  "checkpoint_sha256": Sha256,
  "datastore_heads": [{
    "database_id": DatabaseId,
    "physical_schema": SqlIdentifier,
    "epoch": DatabaseEpoch,
    "last_rotation": MigrationApplyId,
    "checkpoint_sha256": Sha256
  }]
}
```

A rolled-back failing migration identity does not invent an epoch. An earlier
identity that already committed remains committed, so the convergence tail
advances the head and epoch to describe that exact journal prefix before
reporting the original error. If the journal cannot be reconstructed exactly,
the service does not call finalize: the publication stays shrunk, the token
stays claimed and the Datastore stays in that classification-owned `Quiesce`
for identical-bundle recovery.

Finalize combines epoch commit, complete-head recovery and reset release. Control
checks its durable record first: a byte-identical replay, even after `Ready`,
waits on or returns the original result; a changed body is
`409 ClassificationFinalizeConflict`. First application requires the active
unsealed token in its exact classification `Quiesce`, a current-term `Quiesced`
ACK, and matching Database and `migration_apply_id`; `last_rotation` may remain
unchanged. The ACK proves earlier shared writers drained, while durable
`Quiesce` excludes later unrelated writers.

The service supplies every authoritative head as one sorted exact set, and
control validates every Database/schema mapping. A desired+1 head requires
desired=serving and advances desired once. A desired head with serving one behind
must match and rebind its pending transition; with serving=desired it may reflect
journal-only convergence and leaves epoch history untouched. Missing, extra,
backward, larger or two-epoch serving jumps refuse. One transaction persists the
finalize record and snapshot, records or adopts actual pending transitions
through 3.7's shared record, seals the token, and enters same-kind `Restore`, not
`Ready`. The target epoch, rotation and hash must equal its vector entry.

The body deliberately carries no success/failure label. Control needs the
durable converged head, not the ephemeral reason the apply loop stopped, and
the head row plus journal can reconstruct every body field after a crash. The
live request returns its retained engine error after finalization. A recovery
process that did not witness that error returns a deterministic
`RecoveredPartialApply` status and requires the creator to resubmit for any
remaining identities; it does not guess whether the vanished attempt had
succeeded, failed or skipped.

Classification follows 6.2 and 7.2's canonical `Restore`: only its durable
complete-head snapshot authorizes the fresh slot. Under the exclusive fence the
relay serves no rows, decodes the new slot's heartbeat, seeds observed from the
snapshot, and sends `RestoreReady`; that ACK publishes `Ready`. A crash resumes
the same durable phase without timeout. The discarded widen marker is unused.

This loses every incremental change committed during the barrier and refetches
every app sharing the Datastore, including apps whose schema did not change.
That cost is accepted. Keeping the slot would permit plaintext already in WAL
to enter the relay after the field was classified, which is a confidentiality
failure rather than an availability trade.

MEASURED, 18.4, with `zs_cdc` holding `app_alpha.notes (id, body, ssn)`
and `app_beta.items (id, label)`. This is the **shrink** transaction and the DDL
that follows it, which is the pair the post-apply-only order cannot express:

```sql
BEGIN;
  ALTER PUBLICATION zs_cdc DROP TABLE app_alpha.notes;
  ALTER PUBLICATION zs_cdc ADD  TABLE app_alpha.notes (id, ssn);
COMMIT;
-- zs_cdc | app_alpha | notes | {id,ssn}
-- zs_cdc | app_beta  | items | {id,label}        <- untouched
ALTER TABLE app_alpha.notes DROP COLUMN body;     -- ALTER TABLE, no CASCADE, no 2BP01
-- zs_cdc | app_alpha | notes | {id,ssn}
-- zs_cdc | app_beta  | items | {id,label}
```

Three things that transcript rules on. `DROP TABLE` + `ADD TABLE (cols)` in one
transaction changes one member's column list. The `DROP COLUMN` that would have
raised `2BP01` a moment earlier now succeeds, with no `CASCADE` and therefore
without silently unpublishing the table. `pg_logical_emit_message` rides an
`ALTER PUBLICATION` transaction without objection in the separate widen
measurement. And **beta's membership and
column list are byte-identical before and after**, which is the control for (ii):
the same edit expressed as `SET TABLE` computed from alpha's view would have left
`zs_cdc` holding alpha's tables and nothing else.

The marker belongs only to the **widen** transaction, and 8.3 says why. The
widen step is the same two membership statements with `new_wire` in place of
the intersection, followed by the PostgreSQL-18 call in section 8.2.

Four catalog mechanics that the statement "use ADD/DROP instead of SET" hides
were measured on 18.4. Concurrent-decoder atomicity is called out separately
below because its retained measurement is on 17.11:

- **Changing an existing member's column list is `DROP TABLE` then `ADD TABLE`,
  in one transaction.** `ADD TABLE` alone refuses:
  `ERROR: 42710: relation "t" is already member of publication "p1"`,
  `LOCATION: publication_add_relation, pg_publication.c:460`. `ALTER PUBLICATION`
  applies the pair transactionally. A concurrent decoder observed no absent
  interval in the bounded 17.11 measurement in section 13; the 18.4 publication
  bracket gate must repeat that observation before this is target evidence.
- **The drop set must be read from the catalog, not computed from the declared
  set.** `ALTER PUBLICATION ... DROP TABLE` on a relation that exists but is not
  a member is `ERROR: 42704: relation "unpub" is not part of the publication`
  (`PublicationDropTables, publicationcmds.c:1918`), and on a relation that does
  not exist at all is `ERROR: 42P01`. There is no `IF EXISTS`. The reconciler
  reads
  `SELECT tablename, attnames FROM pg_publication_tables WHERE pubname = $1 AND schemaname = $2`
  and diffs against the declared wire set. **`attnames` is the right catalog
  column**, measured directly: it returns `{id,ssn}` for a column-list
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

An engine error still follows 3.7's journal-derived convergence tail. Only
unresolvable recovery debt remains shrunk with `pending_rotation`; 7.6 compares
`cdc_published_columns` with the committed checkpoint.

The bracket also keeps the epoch marker on the right side of the DDL. The
Database advisory lock prevents another migration for this Database from
interleaving the three steps; it does not stop a sibling Database or creator
DML. The publication mutex orders each catalog transaction, while the shared
T4/exclusive-reset fence above decides whether a committed marker precedes slot
destruction or lands after the replacement slot is live. The Database lock axis
is a prerequisite, not current fact: the host currently calls it a project lock
and keys it through `ExecutorConfig.project_id`. The Database rekey specified in
`docs/proposals/2026-08-28-app-database-decoupling.md:698-705` and `:706-739`
must land before this bracket. Shipping the bracket on the app/project key would
permit two apps sharing a Database to interleave it. Changes committed between
DDL and widen are intentionally decoded through the intersection. The marker
commits with the widen and precedes every later change that can use a newly
widened relation shape. No frame crosses the epoch boundary in the wrong
direction.

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

The host also needs identities the current app-keyed request and
`apply_ir_documents` signature do not carry. The prerequisite Database
rekey replaces the app route with the single creator-facing
`POST /v1/databases/{database_id}/migrations/apply` route
(`docs/proposals/2026-08-28-app-database-decoupling.md:628-689`). Caddy routes
that path on the configured control hostname directly to the migration service;
the control service does not forward or re-authorize it.

The migration service verifies the creator bearer against
`Resource::Database { id }`, resolves the owned Database through its existing
control-Postgres connection, and only then constructs this private value:

```rust
struct ValidatedDatabaseApply {
    database_id: DatabaseId,
    datastore_id: DatastoreId,
    physical_schema: SqlIdentifier,
    expected_database_epoch: DatabaseEpoch,
    request: ApplyMigrationsRequest,
}
```

Only `database_id` comes from the authenticated route, and none of the other
binding fields is accepted from creator JSON. The migration service resolves
`expected_database_epoch` from control's worker-visible serving epoch and
resolves the Datastore DSN from `datastore_id`. After taking the Database lock,
it always requires the Datastore id, physical schema and Database id to equal
the Datastore-local authoritative head row.

Epoch validation has one closed recovery exception before the ordinary
stale-binding rejection. If the head is exactly one above the serving epoch,
`pending_rotation IS NULL`, `last_rotation` and `checkpoint_sha256` match the
head's exact effective journal, and control is either still at the prior epoch
or has desired at the head while serving remains prior, T4 committed and its
control obligation is pending. The host performs no shrink or DDL. It reposts
the durable head tuple through the generic epoch-commit endpoint, or through
the active classification-reset token's finalize endpoint, waits for serving
to reach the head, refreshes the validated context, and only then evaluates the
requested bundle. Any larger jump, pending claim, journal mismatch, changed
binding identity or missing exact reset token is recovery debt and refuses
without DDL. In every other case an epoch mismatch is the typed stale-binding
error. This exception must run before ordinary equality or a crash after T4
would make its own recovery path unreachable.

`apply_ir_documents` receives this validated context, derives the shared
publication from `datastore_id`, and emits `database_id` plus the newly
committed epoch in the marker. The accepted cost is one authoritative head read
under the lock; a second internal route, trusting client-supplied binding IDs,
or inferring identity from a schema string is rejected.

The implementation adds `DatabaseEpoch` as a nonzero `u64` newtype in
`zeroship-core`. The private `SchemaEpoch` now at
`crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:97` is moved and
renamed for use by the migration service, control topology, CDC wire and
plugin-db in the same change; the old name is deleted. It also adds
`SqlIdentifier` as an opaque newtype owned by `zeroship-migrate-core`. Its
fallible constructor applies
the platform identifier grammar, rejects NUL and names longer than PostgreSQL's
63-byte limit, and the type does not implement `Display`. Only the SQL renderer
can obtain its contents through the existing identifier-quoting path. Thus
neither type in the command is an implementer-defined placeholder.

The service does hold the material, because the descriptor and the DDL are
folded from the same ordered IR. The current host resolves each document during
preflight (`crates/zeroship-migrate-server/src/apply.rs:879-939`), then
`apply_one_ir_file_postgres` reads and resolves it again at `:748-790`; both
call `resolve_shape_bytes` (`:567-586`). The integration makes the one resolved
artifact typed and reusable:

```rust
struct ResolvedIrDocument {
    filename: String,
    resolved_ir: ResolvedMigrationIr,
    canonical_bytes: String,
    ordered_units: Vec<ResolvedMigrationUnit>,
}

struct ResolvedMigrationUnit {
    identity: MigrationIdentity,
    resolved_ops: Vec<Op>,
}

enum ProjectionDisposition {
    ApplyOps,
    RecordSupersessionNoOp {
        supersedes: BTreeSet<MigrationIdentity>,
    },
}

struct ProjectionDeltaStep {
    identity: MigrationIdentity,
    disposition: ProjectionDisposition,
}

enum ProjectionSelection<'a> {
    PlannedDelta(&'a [ProjectionDeltaStep]),
    CommittedDelta(&'a [ProjectionDeltaStep]),
}
```

Discovery order is preserved. `apply_ir_documents` prepares the slice once,
before preflight, and passes it to preflight and `run_apply`. Each file is read,
deserialized, policy-resolved and canonically serialized exactly once into that
type. `ResolvedMigrationIr` has a private constructor, so unresolved ops cannot
enter this path. The same lowering pass records the typed, policy-resolved
`Op` slice beside every lowered unit's exact version, checksum and kind. The
private container, not a parallel projection DSL, is what prevents unresolved
ops from entering the fold. `ordered_units` is in execution order and has no
duplicate identity; it is a `Vec` rather than a set because a committed prefix
inside one source document must remain reconstructible.
`preflight_ir_documents` and `apply_one_ir_file_postgres` accept references to
the prepared values instead of paths and do not repeat the work.

The renderer exposes a typed output-only projection rather than asking the host
to reverse-engineer `CollectionDescriptor`:

```rust
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct QualifiedRelation {
    pub schema: SqlIdentifier,
    pub table: SqlIdentifier,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct QualifiedField {
    pub relation: QualifiedRelation,
    pub logical_field: LogicalFieldIdentifier,
}

pub struct WireProjection {
    pub relations: BTreeMap<QualifiedRelation, WireRelationProjection>,
    pub newly_protected_fields: BTreeSet<QualifiedField>,
}

pub struct WireRelationProjection {
    pub published_columns: BTreeSet<WireColumnIdentifier>,
    pub replica_identity_columns: BTreeSet<WireColumnIdentifier>,
}

pub struct SchemaExport {
    pub artifacts: GeneratedArtifacts,
    pub collections: BTreeMap<String, CollectionDescriptor>,
    pub wire_projection: WireProjection,
}
```

`SchemaExport` currently contains only `artifacts` and `collections`
(`crates/zeroship-migrate-core/src/render/gen_types.rs:535-546`).
`CollectionDescriptor` carries declared fields and indexes but no physical
storage mapping or folded primary key
(`crates/zeroship-migrate-core/src/render/declarative.rs:360-390`). It is
therefore not the wire projection. A new
`render_schema_export_resolved(documents, checkpoint, selection)` constructs
`wire_projection` during its existing single fold. Both selection variants are
deltas relative to `checkpoint.net_applied`, never a request-sized replay over
`checkpoint.folded_schema`. `PlannedDelta` is the ordered candidate identities
returned by the engine's own preflight classifier after applying its
versioned, repeatable-checksum and squash semantics to the current effective
journal. An identity already represented by the checkpoint is absent; an
unchanged repeatable is absent; a changed repeatable is present once with its
new checksum. `CommittedDelta` contains the ordered steps the post-run
effective journal proves newly recorded.

The disposition is load-bearing. On a fresh Database a squash is `ApplyOps` and
its combined `up` is folded once. On an existing Database where every
superseded migration is already effective, the engine journals the squash and
its supersession edges without running `up`
(`crates/zeroship-migrate-core/src/ops/squash.rs:11-19` and `:281-288`).
That step is `RecordSupersessionNoOp { supersedes }`: the renderer verifies the
exact superseded identities are represented, applies no `resolved_ops`, leaves
`folded_schema` byte-identical, and takes the new `net_applied` set from the
engine's effective-journal result. It never infers disposition merely because
an identity is newly visible in the journal. The engine exposes the same typed
disposition from preflight and from journal recovery, so a crash cannot turn a
record-only squash into executed schema ops.

The renderer applies `resolved_ops` only for `ApplyOps` steps, in original unit
order; membership never becomes execution order. For each physical
relation it maps declared fields through the same private `field_storage` decision
(`gen_types.rs:289-325`) and unions the folded primary-key columns. It refuses a
relation with an empty replica identity and verifies that every identity column
is present in `published_columns` and safe under 3.1. Its fold-local
`ProjectionFieldKey` map starts from the checkpoint, survives table and column
renames, and is discarded only after it derives the protected-transition set;
it is renderer provenance, not a wire identifier.

`WireColumnIdentifier` and `LogicalFieldIdentifier` have private fields, no
`Display` implementation and fallible renderer-only constructors.
`WireColumnIdentifier` can be constructed only from a folded
`wire_exposure` result and refuses `rawColumn` identities and
`ExcludedProtected` fields. The publication SQL renderer accepts that type
directly. A general `String` or even a general `SqlIdentifier` therefore cannot
be substituted for a published column by the migration host.
`QualifiedRelation` likewise keeps schema and table separate through quoting.

The existing public `render_schema_export` remains the build-time entry: it
performs policy resolution once and delegates to
`render_schema_export_resolved`. The migration host calls the resolved entry
directly, so it does not pass already-resolved ops back through
`resolve_create_table_policy` at `gen_types.rs:556-581`. Both paths share one
fold implementation; neither scans SQL text or reconstructs a descriptor.

**The epoch producer is part of this integration.** No such producer or system
schema exists today
(`crates/zeroship-plugin-db/src/auth/bootstrap.rs:15-18`). Datastore
provisioning creates exactly one reserved table:

```text
__zeroship_admin.database_heads {
  database_id text primary key,
  physical_schema name unique not null,
  epoch bigint not null check (epoch > 0),
  projection_checkpoint bytea not null,
  checkpoint_sha256 bytea not null check (octet_length(checkpoint_sha256) = 32),
  last_rotation uuid not null,
  pending_rotation uuid,
  pending_base_epoch bigint,
  check ((pending_rotation is null) = (pending_base_epoch is null))
}
```

The schema and table are owned by an operator no-login role and contain no
function. `PUBLIC`, app and worker roles receive no schema usage or table
privilege. The relay's Datastore management login receives only schema usage
and column-level `SELECT` on `database_id`, `physical_schema`, `epoch`,
`last_rotation` and `checkpoint_sha256`; it cannot read
`projection_checkpoint` or pending claims and cannot write anything. That
minimal read is solely the slot-reset handshake in 6.2. The migration service
writes the row through the existing privileged
`migrate_server.provision_database_url`, which is explicitly a superuser DSN today
(`deploy/compose/docker-compose.yml:473-501`). That is permissible because the
migration service is the separate privileged process; this proposal does not
pretend the current credential is a least-privilege login.

`projection_checkpoint` is the canonical encoding of
`ProjectionCheckpoint { folded_schema, net_applied:
BTreeSet<MigrationIdentity> }`. A `MigrationIdentity` contains the engine's
version, checksum and versioned/repeatable kind. It is the exact effective
journal set after squash and repeatable semantics, not a count and not an
assumed filename prefix.
`checkpoint_sha256` is the raw 32-byte SHA-256 of that exact encoding and is
maintained atomically on every checkpoint change. It lets the relay attest the
commit tuple without receiving the folded schema.

Database provisioning takes the shared Datastore fence and inserts epoch one,
an empty checkpoint, its hash and the provisioning rotation id. Under the
Database lock, an apply reads the row `FOR UPDATE` and compares its exact set
with the engine journal. A mismatch is recovery debt and must be reconciled from
matching resolved documents before new DDL can run. This checkpoint is what
makes `OnUnmet::Skip` implementable: skip leaves one migration pending while an
unrelated later migration may apply
(`crates/zeroship-migrate-core/src/apply/executor.rs:601-605`), so "first N
documents" is not a valid schema boundary.

T1 writes a typed `MigrationApplyId` UUID, returned by classification Prepare or
locally minted for an ordinary apply, into `pending_rotation` with the
current epoch in `pending_base_epoch` in the same transaction that reaps E-1.
A retry adopts an existing claim only when its base epoch still equals the head
and its journal delta is representable by the supplied exact-checksum
documents; otherwise it refuses. T4 clears both fields atomically with the head
advance, checkpoint hash and `last_rotation`. If the engine journal does not
move and there was no recovery debt,
the host runs a no-rotation repair transaction: widen back to the exact
checkpoint projection and clear the claim atomically, without minting roles,
incrementing the epoch, moving `last_rotation` or emitting a marker. A retry
after a crash between shrink and that repair adopts the claim and performs the
same repair. An all-skip or empty apply therefore cannot leave the publication
narrowed and never increments the epoch. A journal that moves only through
`RecordSupersessionNoOp` uses the distinct journal-only convergence transaction
in step 6 below: it moves `last_rotation` and the effective-set checkpoint but
does not rotate roles or epoch.

Before DDL, the renderer applies `PlannedDelta` to a copy of the checkpoint only
to derive a conservative shrink projection and `newly_protected_fields`
candidates. A precondition-skipped migration may still be in that plan and
therefore cause an unnecessary temporary shrink or Datastore reset, which is
the accepted availability cost. Resubmitting a full bundle whose identities
are already effective produces an empty plan; replaying `createTable` or any
other old op into the folded checkpoint is forbidden. The conservative fold
can never widen the live publication.

Whether the engine loop succeeds or stops at its first error, the host retains
that result and enters one convergence tail. It rereads the exact effective
journal set and computes its typed ordered delta from the checkpoint. It advances a
second checkpoint only with resolved migrations whose exact version and
checksum the journal says applied. Skipped, failed and unapplied identities are
absent; earlier committed identities remain present even when they came from
the same source document as the failure. The exact newly committed ordered step
delta is passed as `ProjectionSelection::CommittedDelta`, so neither the
document boundary nor filename order can erase that prefix. The resulting
checkpoint stores the engine's complete new effective set; squash and
repeatable entries use the engine's disposition and verdict rather than
filename order. If a newly journaled `ApplyOps` identity has no byte-identical
prepared document, recovery refuses with `ProjectionHistoryUnavailable` and
leaves the publication shrunk and the claim pending; the creator must retry
with the original bundle. This fail-closed recovery requirement is the cost of
not storing every migration body in a second journal.

**Locked host integration.** The bracket is inserted inside the project-lock
bracket in `apply_ir_documents`, which acquires at
`crates/zeroship-migrate-server/src/apply.rs:346` and releases at `:440`.

**THIS PARAGRAPH NAMED `apply_bundle_ir_postgres` AND CALLED `run_apply` THE
"OBSOLETE OUTER SEAM" UNTIL 2026-08-30, AND BOTH HALVES ARE NOW WRONG.**
`apply_bundle_ir_postgres` no longer exists - the rollback-guard work inlined it
into `apply_ir_documents` - and `run_apply` is not outside anything: it is CALLED
at `:382`, inside the bracket. The lock comment at `:353-357` states why that
bracket is where it is: it "binds all four facts the deploy ledger relies on: the
catalog snapshot used to lower, the complete supplied manifest set, the journal
coverage verdict, and the terminal ledger timestamp. Releasing before
`mark_applied` would let an older concurrent request stamp a newer `applied_at`
after a later schema had already completed."

So the DECISION survives - the bracket belongs where the lock is held, not around
a wider seam - but every name in its original wording pointed at code that a
sibling branch deleted. Nothing caught this: the citation gate rules on paths and
on a line being inside its file, never on symbols, and the six `apply.rs:NNN`
citations in this document all still land inside a file whose every line moved.

That function previously acquired the project lock once at
`crates/zeroship-migrate-server/src/apply.rs:682-688`, ran its body at
`:690-718`, and releases unconditionally at `:720-740`. After the prerequisite
Database rekey, the same locked host seam remains and the lock key is
`database_id`, as required by
`docs/proposals/2026-08-28-app-database-decoupling.md:698-705`. The current
empty-directory return at `apply.rs:675-678` moves inside this locked recovery
body, after the head and journal inspection. An empty retry can therefore repair
a stranded no-rotation shrink; it refuses unresolved journal debt that requires
missing documents. Inside that body, in order:

1. Read `database_heads FOR UPDATE`, the live publication and the exact engine
   journal set; run the head-ahead recovery branch above before ordinary
   binding equality, then validate the checkpoint.
2. Render the conservative requested projection from the checkpoint plus the
   engine's `PlannedDelta`. If `newly_protected_fields` is nonempty, complete 3.6's
   durable Datastore reset barrier before any local side effect.
3. Claim or adopt `pending_rotation`, perform that design's T1 reap of epoch
   E-1 roles, then commit the publication shrink to
   `live_wire INTERSECT requested_wire`.
4. Apply each prepared document through `apply_one_ir_file_postgres` with
   `LockMode::AlreadyHeld`, as the loop already does at `:695-710`. Stop at the
   first error, retain it, and continue to the convergence tail.
5. Reread the journal regardless of that result and render the exact committed
   projection from the prior checkpoint plus only its typed ordered delta.
6. If the delta is empty, run the no-rotation repair. If it contains only
   `RecordSupersessionNoOp` steps, run a journal-only convergence transaction:
   keep the folded schema, publication, roles and epoch unchanged; store the
   engine's new exact `net_applied` set and checkpoint hash, move
   `last_rotation` and clear the pending claim. Emit no marker and create no
   control epoch obligation.
   Otherwise, in one T4
   transaction, mint epoch E+1 bind roles and grants, widen this Database's
   members to that exact projection, update the head epoch, checkpoint and hash,
   and emit the returned `(database_id, epoch)` marker. On an ordinary apply,
   before opening T4 or taking the publication mutex, the host takes the shared
   Datastore fence and revalidates control `Ready`; on a classification apply,
   the exact unsealed token authorizes this T4 only in `Quiesce` after
   `Quiesced` established exclusive. The same choice applies to the head-writing
   no-rotation and journal-only branches above. The durable reset state
   preserves exclusion if that relay later dies. All effects commit or abort
   together, matching the prerequisite T4 contract at
   `docs/proposals/2026-08-28-app-database-decoupling.md:706-739`.

The ordinary shared fence is released immediately after T4, before the blocking
control POST. A reset that acquires exclusive afterwards must recover that
committed head even if the process dies before POST. Conversely, a T4 that did
not commit before exclusive acquisition releases shared without opening T4 or
taking the publication mutex and returns retryable. It may wait fence-free, then
reacquire and revalidate only after durable `Ready`, when its marker enters the
fresh slot. This is the complete ordering proof; neither the Database lock nor
the publication transaction mutex covers a sibling Database.

The edge routes the creator apply request directly to the migration service, so
an apply response cannot update control by observation. T4 therefore creates a
durable epoch-commit obligation in the head row. The WAL marker is the normal
retained-slot delivery mechanism; the complete-head reset handshake is its only
explicit-gap substitute. When no classification-reset token is active,
the migration service sends the following before returning either success or a
retained engine error whose committed prefix advanced the epoch:

```text
POST /internal/v1/cdc/databases/{database_id}/epoch-commits
Authorization: Bearer <migration-service assertion>
Content-Type: application/json

{
  "database_id": DatabaseId,
  "datastore_id": DatastoreId,
  "epoch": DatabaseEpoch,
  "last_rotation": MigrationApplyId,
  "checkpoint_sha256": Sha256
}
```

It uses the required `ZEROSHIP_CONTROL_URL` and a single-use service assertion
for `spiffe://zeroship.ai/svc/control`. Control consults a durable record keyed
by `(database_id, epoch)`, used only for E-to-E+1 and shared by this endpoint,
classification finalize and `ResetHeads`. It binds the complete tuple to an
ordinary topology revision or reset generation plus Restore revision; an exact
match waits or returns, while a changed tuple is `409 EpochCommitConflict`.
Absent a record, this endpoint verifies the mapping, requires
desired=serving=E, and atomically stores the E+1 tuple and record, advances
desired and increments `topology_revision`. A complete-head snapshot creates a
reset-bound record for desired+1, or, when desired=E+1 and serving=E, requires
the exact pending tuple and rebinds its unfinishable marker revision to the reset.
Stable desired=serving heads leave epoch history untouched. Ordinary authority
completes on the marker-observed/topology-desired ACK; reset authority completes
on `RestoreReady`. Either advances worker-visible serving and returns
`200 { "topology_revision": R, "serving_database_epoch": E + 1 }`.
An epoch other than the recorded epoch or exactly next is
`409 EpochCommitOutOfOrder`.

The apply returns only after durable relay ACK and serving promotion. Lost T4
request or response retries the exact head tuple without another epoch; before a
later T1, serving must equal the head or that obligation replays and new DDL
refuses. Section 6.2 closes both marker/topology arrival orders. Control and the
relay are therefore on the response path, while committed DDL stays retryable.
An active classification token uses 3.6's finalize instead of this endpoint.

The post-lock call to `reconcile_app_publication` at
`crates/zeroship-migrate-server/src/apply.rs:461` is deleted. There is one
publication mutation path, not a second repair pass outside the lock.

The requested fold and committed fold have different explicit jobs. The first
can only subtract and can be over-conservative; the second is authoritative for
widen. Shrink intersects the requested result with the LIVE set read from
`pg_publication_tables`, because the catalog is what PostgreSQL enforces and
`ALTER PUBLICATION ... DROP TABLE` has no `IF EXISTS`.

Two things this forbids. The service must not construct
`__zs_raw__<field>` or any historical `_masked` name: physical storage comes
from the fold. And it must not fetch the descriptor over HTTP from the build:
that would reintroduce the staleness window 12.3 rejects, over a network hop, on
the path that gates creator writes.

`descriptor_sha256` stops being the only thing tying the projection to the
descriptor: `:64-68` says outright that the hash "proves ordering, not truth",
because a creator who hand-edits both generated files can make them agree about a
lie. A projection folded server-side from the ops the server is about to apply is
not client-declared.

The accepted cost is a persisted renderer checkpoint, two incremental folds,
two publication catalog transactions inside the Database-lock lifetime, and a
longer lock hold. If DDL or T4 fails, the publication remains safely narrowed
until recovery. That availability loss is accepted in exchange for never
publishing a skipped, pending, undeclared or raw column.

---

## 4. Which code is extracted, deleted or retained

The test applied is the one the index already adopted for the crate split
(`...-00-index.md:192-203`): a crate boundary must buy a dependency the compiler
refuses to invert, a separate compilation unit, or an artifact another process
consumes. Here it buys the third, which is the strongest of the three.

**New: `crates/zeroship-cdc`** (binary + lib), plus a small shared crate for the
wire types (see 5.2).

| file | verdict |
| --- | --- |
| `wal_consumer.rs` (1,440 lines) | **Split and rewrite; do not move the file.** It imports plugin-db's broker, `ChangeEvent` and `DbError`, so a literal move would make the relay depend on the V8-bearing plugin crate. Extract the pgoutput decode algorithm, `RelationEntry` and its `primary_key_index` (`:256-260`), replication parameter check, backoff policy (`:710-843`) and fatal classification (`:743-760`) behind relay and wire-owned types. Delete the plugin-only `SUPPRESSED_APPS`, suppression guard and `emit_local` (`:66-164`) with the Postgres local-emit path below. No `zeroship-cdc -> zeroship-plugin-db` dependency is allowed. |
| `replication.rs` (908 lines) | **Split and rewrite; do not move the file.** It imports plugin-db's `DbError` and app/worker naming. Extract exact-slot lifecycle and health-query algorithms behind relay errors. Replace app-keyed names with `datastore_publication_name` and `datastore_slot_name` (3.6); delete `worker_slot_name`, its prefix (`:114-132`) and per-worker drop entry points. `ensure_datastore_slot` serves one Datastore. `drop_datastore_slot` derives and verifies one exact name plus `database = current_database()` and is reachable only through the fenced classification, identity or invalidation reset handshake. Broad prefix enumeration remains forbidden. |
| `slot_reaper.rs` (593 lines) | **Deleted.** Section 2.2. |
| `change_stream_pg.rs` (315 lines) | **Deleted.** `SharedExit` (`:24-64`) and `WalConsumerHandle` (`:66-108`) are the process-local supervision of a task that no longer exists in the worker. `PgChangeStream::spawn_consumer` (`:170-264`) and `deprovision` (`:162-164`) go away with the per-worker slot. `pause_broker` / `engage_schema_pending` (`:269-278`) survive as broker functions, which is what they already delegate to. |
| `replication_ops.rs` (47 lines) | **Deleted.** `db.replication.watchdog()` exposes slot diagnostics to creator JavaScript for slots the creator will no longer own. Its own module comment already narrowed it once (`:1-7`). |
| `broker.rs` (1,937 lines) | **Stays in `zeroship-plugin-db`.** It is the in-process routing table, and its consumers are V8 subscription wrappers on the same thread. The unsafe value-carrying `ws_frame_for_change` and `ws_frame_for_control` helpers are already deleted. Keep `message_to_json` (`:927-960`) and the current safe `ws_frame` (`:1007-1027`); both emit creator-visible names and no row values. |
| `read_set.rs` (659 lines) | **Stays.** Capture happens inside `ctx.db.find` in the isolate, gated on the procedure kind (`:33-39`). It cannot leave the process that runs the query handler. |
| `cdc_lifecycle.rs` (523 lines) | **Stays**, reshaped. The refcounted per-app lease (`acquire` at `:87-111`, `release` at `:113-157`) still decides when this worker needs `app_id`'s stream. It does **not** currently expose the active set: `acquire` mutates a private map one app at a time. Add an atomic snapshot accessor returning `cluster_id`, `database_id`, `database_epoch` and `grant_generation` for every leased app. `RunningConsumer::Postgres(WalConsumerHandle)` (`:25-30`) becomes a handle on the per-cluster relay subscription rather than on a local task. |
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
- the `pending_emits` slot on the isolate context, its import at
  `transaction/mod.rs:143`, the sole production cleanup call at `:527`, and
  the three test helpers at `lib.rs:768-787`.

That last bullet is why the polarity has to be read carefully before scoping the
work: this is not a three-function edit inside `exec.rs`, it reaches transaction
cleanup and the isolate test surface.

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

`ChangeStream` (`crates/zeroship-plugin-db/src/backend/mod.rs:840-883`) survives
as a capability trait with a changed Postgres implementation: `spawn_consumer`
becomes "subscribe to the relay". `deprovision` removes only this worker's local
lease and performs 5.2's immutable make-before-break response replacement
without the app; it cannot delete a shared relay ring or revoke a Grant. Global
forget/revoke is control topology and must pass 6.2's revision barrier.

---

## 5. The worker/relay wire contract

The information represented by `ChangeEvent` (`broker.rs:81-119`) crosses a
process boundary now, but that plugin-owned Rust struct does not. Today it is
passed by `Arc` inside one process (`broker.rs:756-759` wraps it once and clones
the `Arc` per subscriber), so it has never needed an encoding. The leaf wire
crate owns the new process contract and the worker translates it into the
smaller target `ChangeEvent`; the relay never imports the broker type.

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
regression: every worker would ingest every tenant's feed before discarding
almost all of it.

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
per-app topic and (d) fixed is the candidate to re-evaluate; 6.3 and 12.1 state
why this version accepts a failover gap instead.

### 5.2 The contract

**Encoding.** A new leaf crate `zeroship-cdc-wire`, no I/O, no V8, depended on by
both `zeroship-cdc` and `zeroship-plugin-db`. Each frame is exactly
`u32_be(frame_len) || u8(tag) || payload`, where `frame_len` counts the tag and
payload. Integers inside a payload are big-endian; byte strings and UTF-8
strings have a `u32_be` length; enum discriminants are `u8`. A zero length, a
length over `1_048_576`, invalid UTF-8 in a string field, trailing payload
bytes, or an unknown tag is a fatal protocol violation and closes the response.
The request and `Hello` must name the one exact supported handshake version;
there is no best-effort decode of another version. `max_frame_bytes =
1_048_576` and `max_cell_bytes = 524_288` are protocol constants, not tunable
per relay, so every encoder makes the same `Gap(OversizeValue)` decision.

The primitive layout is closed. `wire_version` is `u16`; `timeline`,
`change_index` and every vector count are `u32`; `systemid`, LSNs, terms,
epochs, generations, sequence numbers and `prime_batch_id` are `u64`. An LSN is its raw
PostgreSQL `XLogRecPtr` integer, not `0/16B6C50` text. `Bytes` and `Utf8` are
`u32 length || bytes`. A typed id is its canonical ASCII rendering in that same
string encoding, bounded to 64 bytes and parsed by the expected concrete id
type; a wrong prefix or noncanonical base62 form is fatal. `Vec<T>` is
`u32 count || T...`. `Option<T>` is `u8(0)` or `u8(1) || T`. Structs are the
concatenation of fields in the order printed in this section, with no field
numbers or padding. The same `0x00` and `0x01` bytes are the only Boolean
encodings. Counts, lengths and multiplication are checked before allocation.
`pk` is `Vec<CellValue>` in replica-identity order; it may contain `Value` or
`Null` but never `Unavailable`.

`LeaderTerm`, `DatabaseEpoch` and `GrantGeneration` are encoded as `u64` but
their typed constructors admit only `1..=i64::MAX`, matching the PostgreSQL
`bigint` columns that persist them. `systemid` and LSNs retain the full `u64`
domain. A decoder refuses an out-of-domain value before comparing or storing
it; there is no wrapping cast.

The initial protocol is `wire_version = 1`. Its frame tags are fixed:
`Hello=0x01`, `RelationPrime=0x02`, `Registered=0x03`, `Resync=0x04`,
`AppReset=0x05`, `Relation=0x06`, `Change=0x07`, `Gap=0x08`,
`Truncate=0x09`, `Epoch=0x0a` and `Heartbeat=0x0b`. Nested closed enums are also
fixed in v1: registration outcomes `Resumed=0x01, Reset=0x02,
Rejected=0x03`; `CellValue` `Value=0x01, Null=0x02,
Unavailable=0x03`; operations `Insert=0x01, Update=0x02, Delete=0x03`;
the one `ResetReason` used by `Registered::Reset`, `Resync` and `AppReset`
follows `Initial, RelayFailover, RingOverrun, DatabaseEpochChanged,
GrantRebound, AppDegraded, AppReactivated, DatastoreReconnect,
SlotInvalidated, SystemIdentityChanged, ClassificationChanged` starting at
`0x01`; registration
reject codes follow `AppNotInTopology, GrantInactive, DatastoreUnavailable,
StaleExpectedBinding, EpochPending, CursorBindingMismatch, CursorAhead`
starting at `0x01`; and
`GapReason` follows `OversizeValue, UnrepresentableType` starting at `0x01`.
Boolean bytes are only `0x00` and
`0x01`. Any other discriminant is a fatal v1 decode error, not an
`Unknown` variant.

The framing borrows pgoutput's own solution to the same problem, because the
problem is the same: column names must not repeat per row.

| frame | payload |
| --- | --- |
| `Hello` | wire version, relay id, leader term, `cluster_id`, `systemid`, `timeline` |
| `RelationPrime` | unsequenced connection metadata: `(prime_batch_id, app_id, database_id, database_epoch, grant_generation, relation_generation, collection: Utf8, columns: Vec<(name, is_replica_identity)>)` |
| `Registered` | registration generation plus `Vec<RegistrationOutcome>`, one app-identified outcome per requested app in request order |
| `Resync` | unsequenced, connection-local `(prime_batch_id, prime_relation_count, app_id, database_id, database_epoch, grant_generation, ResetReason, accepted_cursor)` |
| `AppReset` | sequenced app-wide `(ResetReason)` |
| `Relation` | sequenced `(relation_generation, collection: Utf8, columns)` |
| `Change` | sequenced `(relation_generation, op, commit_lsn, change_index, pk: Vec<CellValue>, values: Vec<CellValue>)` positional against the primed or last `Relation` |
| `Gap` | sequenced `(collection: Utf8, pk, commit_lsn, change_index, reason)` - see 5.6 |
| `Truncate` | sequenced `(collections: Vec<Utf8>)` - see 5.6 |
| `Epoch` | sequenced `(database_epoch)` - see section 8 |
| `Heartbeat` | `(datastore_id, last_confirmed_lsn)` |

`columns` is `Vec<Utf8 name || Bool is_replica_identity>`.
`RegistrationOutcome` is `AppId || u8 outcome_tag || outcome_body`. A
`Resumed` body is
`u64 prime_batch_id || u32 prime_relation_count || DatabaseId || u64 database_epoch ||
u64 grant_generation || AppCursor`. `Reset` adds one `ResetReason` immediately
before `AppCursor`; `Rejected` contains only its
`RegistrationRejectCode`. A `CellValue::Value` is its tag followed by `Bytes`;
`Null` and `Unavailable` have no body. These tagged-union rules apply inside
both `pk` and `values`. All other tuples in the table encode left to right using
the closed primitives above.

For `Change`, the `pk` polarity is 3.4's closed rule. On an identity-changing
UPDATE the worker derives the new identity from the replica-identity positions
in `values`; an unavailable or malformed identity makes the row a `Gap` rather
than guessing. No before-image field exists in v1.

Identity validation is part of decoding, not caller convention. A `Registered`
must repeat the request's exact `registration_generation` and contain exactly
one outcome for every app in its request shard, in request order, with no extra
app. Every accepted cursor must repeat that request's
`app_id` and `worker_id`, the `relay_id` and `leader_term` from this response's
`Hello`, and the Database, epoch and Grant generation in its outcome. Every
`RelationPrime` app must occur in the request and its binding must equal the
terminal outcome for that batch. A rejected app must have no staged prime.
For a later `Resync`, its app must already be selected on this response, and its
binding and accepted-cursor identities must equal that selected binding and
`Hello`. Any mismatch is a fatal response decode error before cache or cursor
mutation.

`Change` carries values positionally against a `relation_generation`, so a row
costs its values plus a small header rather than its values plus its column
names. A cell over `max_cell_bytes`, or a complete encoded `Change` over
`max_frame_bytes`, is replaced by the corresponding keyed `Gap`; a `Gap` that
cannot fit triggers `AppReset(AppDegraded)` and app quarantine as specified in
5.6 because even its collection and primary key cannot be represented.
The migration projection builder refuses a relation whose encoded `Relation`
or `RelationPrime` can exceed `max_frame_bytes`, so an accepted schema cannot
discover that failure while streaming. `Truncate.collections` is split in
relation-id order across as many sequenced `Truncate` frames as required.
`Hello`, `Epoch`, `Heartbeat` and `AppReset` are fixed-size apart from bounded
typed ids. The 1,024-app request limit keeps `Registered` below the frame
ceiling. There is no unspecified oversize path for a non-`Change` frame.

Every sequenced frame has the common envelope
`(app_id, grant_generation, seq)`, encoded before its frame-specific payload,
and lives in the one ring for that exact
`(app_id, grant_generation)` binding. `AppReset`, `Relation`, `Change`, `Gap`,
`Truncate` and `Epoch` are the complete sequenced set. `Registered` and
`Resync` are connection-local controls and consume no app sequence. This is
necessary when two workers subscribe to one app: putting one slow worker's
reset in the shared sequence would either reset the healthy worker or leave a
hole in its cursor. `AppReset` is different: it records a condition that
invalidates every worker for the app, such as slot recreation or quarantine, so
one shared sequence position is correct.

**`(commit_lsn, change_index)` is the dedup key**, for the reasons measured in
6.3. `commit_lsn` is the LSN of the transaction's commit record and is available
on the FIRST frame of the transaction, not only at the end:
`PgOutputMessage::Begin` carries `final_lsn`, documented as "LSN of the commit
record (NOT the begin record)"
(`libs/compio-postgres/src/replication.rs:1879-1886`), and `Commit.commit_lsn`
(`:1891-1892`) is the same value. So the relay stamps it as it decodes, without
buffering the transaction. `change_index` is the 0-based pgoutput DML ordinal:
the decoder increments it exactly once for every decoded `Insert`, `Update` or
`Delete`, before projection, Grant lookup or fan-out, and resets it at every
`Begin`. Every fan-out copy of one decoded row carries the same ordinal, and a
row with zero active Grants still advances the counter. Topology changes
therefore cannot renumber a replay. `relation_generation` is a monotonic `u64`
that is never reused within
`(relay_id, leader_term, app_id, grant_generation)`, including across a
same-term Datastore reconnect or slot recreation. Losing that counter is loss
of sequence state and forces self-demotion; a new term may start again at one.

`CellValue` is three-way - `Value(Bytes) | Null | Unavailable` - not
`Option<Bytes>`. 5.6 says why collapsing the last two is the same class of
mistake section 3 exists to prevent.

`Hello` carries `systemid` and `timeline` because those invalidate the numeric
dedup watermark (6.4). `leader_term` independently invalidates incremental
subscriber state and forces `Registered::Reset(RelayFailover)`.

The worker keeps one term fence per `cluster_id`; every response frame and broker
item retains its exact `(cluster_id, relay_id, leader_term)`. Broker admission and
subscriber delivery take that cluster fence's shared guard through one
non-yielding side effect. A valid higher pair from control or a verified `Hello`
makes the worker acquire exclusive, wait for every in-flight guard, cancel lower-
term responses, purge their queues, publish local `Resync(RelayFailover)`, and
advance the pair before acknowledging control. A paused guard therefore prevents
the ACK instead of resuming after it. Lower terms close, same-term/different-relay
is split brain, and another cluster's responses are untouched. Section 6.2 makes
this local quiescence a prerequisite to the control transition, not a timed
assumption.

Not `serde_json`. A JSON `HashMap<String,String>` per row is precisely what
`new_tuple` is today, and that shape is what let a bag of physical column names
become the thing on the wire.

**Transport.** The worker sends:

```text
POST /internal/v1/cdc/subscribe
Authorization: Bearer <fresh worker service assertion>
Content-Type: application/vnd.zeroship.cdc.v1
Accept: application/vnd.zeroship.cdc.v1
```

The body is one v1 binary
`SubscribeRequest` with a 1 MiB request limit and at most 1,024 apps. A
successful response is `200` with the same content type; `Hello` is always its
first frame, exactly once, followed by primes and `Registered`. The worker then
reads frames until the body closes. The pieces already exist and are already
compio:
`ntex = { version = "3", features = ["compio", "rustls"] }` is the relay HTTP
server the peer service `zeroship-migrate-server` already uses
(`crates/zeroship-migrate-server/src/api.rs:3-5`); the workspace currently
enables only `compio` (`Cargo.toml:45`), so `rustls` is an explicit relay-crate
feature addition. `cyper = { version = "0.8",
default-features = false, features = ["rustls", "json", "stream"] }`
(`Cargo.toml:48`) is the client, its `stream` feature is on, and
`crates/zeroship-worker/Cargo.toml:39` already names it. Authentication is a
single-use service assertion with exact audience
`spiffe://zeroship.ai/svc/cdc`
(`crates/zeroship-core/src/service_assertion.rs`), which is the tree's existing
service-to-service identity mechanism. The relay verifier uses
`zeroship_authn::service_replay::PostgresReplayStore` against the shared
`service_authn.service_assertion_replay` table in the coordination database.
It never uses `InMemoryReplayStore`: the core module explicitly limits that
implementation to a single-replica callee, while leader failover makes the
relay replicated. Failure to claim a `jti` is
`AuthError::StoreUnavailable` and refuses the subscription before its body is
parsed. The accepted cost is that coordination PostgreSQL availability is also
an authentication dependency, in exchange for assertions remaining single-use
across replicas and leader terms.

Every `cdc_endpoint` is `https://`. TLS terminates in the relay process, not at a
load balancer followed by a plaintext hop. The worker's rustls client verifies
the operator CA from required `ZEROSHIP_CDC_CA_BUNDLE` and the endpoint DNS
name; the relay reads its certificate and key from
`ZEROSHIP_CDC_TLS_CERT_FILE` and `ZEROSHIP_CDC_TLS_KEY_FILE`. Plain HTTP,
disabled certificate validation and an IP address absent from the certificate
are startup or connection refusals. Service assertions authenticate the worker;
TLS separately keeps creator rows confidential in transit. The accepted cost is
certificate issuance and rotation for this internal service. The worker builds
Cyper with a custom rustls `ClientConfig` rooted only in that CA bundle; adding
the bundle to host-global roots is forbidden.

HTTP failures use `application/problem+json` with only a closed `code`:
`401 AuthenticationFailed`; `400 DuplicateApp`,
`NonMonotonicRegistrationGeneration`, `ConflictingShard` or `MalformedRequest`;
`406 NotAcceptable`; `409 ClusterMismatch`; `413 RequestTooLarge`;
`415 UnsupportedMediaType`; `426 UnsupportedWireVersion` plus
`ZeroShip-CDC-Wire-Version: 1`; and `503 NotLeader` or
`AuthenticationStoreUnavailable`, `TermAuthorityUnavailable` or
`ConnectionCapacityExceeded`. None begins a binary body or emits `Hello`.

The worker retries `NotLeader`, `AuthenticationStoreUnavailable`,
`TermAuthorityUnavailable` and
`ConnectionCapacityExceeded` with full jitter starting at 250 ms and capped at
5 seconds, minting a fresh assertion for every attempt. Other HTTP errors are
terminal until topology or code changes; they are never converted into an
empty successful registration.

A same-term transport EOF, timeout or retryable I/O failure before terminal
`Registered` discards every staged prime and leaves each previously selected
app feed unchanged. A validated higher-term `Hello` is the deliberate
exception: its term fence has already cancelled every lower-term feed, so a
later pre-`Registered` failure leaves those apps reconnecting and can never
restore the old term. After `Registered`, the worker freezes each last
processed `AppCursor`, keeps local subscriptions in reconnecting state, mints a
new registration generation because the body changed, and opens a
make-before-break replacement with the same 250 ms to 5 second full jitter;
slow-consumer and term-fence closures use this path. Certificate,
hostname and trust-bundle failures are configuration-fatal and wait for
topology or certificate change. A frame decode violation is an implementation
or peer invariant failure: it closes the response, drops staged state, surfaces
the exact closed decoder code to operators without row bytes, and does not
retry the same endpoint and wire version until topology revision or process
version changes. Thus an invalid peer cannot create a hot reconnect loop.

Push rather than poll, because the point of the service is to reduce end-to-end
latency relative to WAL-to-worker, and a poll interval is a latency floor chosen
in advance. Push without a credit scheme puts the whole slow-consumer question on
the relay, which is where 5.4 answers it.

**Cluster routing and registration request.** The control-plane topology
snapshot gives the worker `(app_id, cluster_id, cdc_endpoint)` for each active
Grant. The worker groups leases by `cluster_id`, sorts each group by canonical
`app_id` bytes, and splits it into deterministic contiguous shards of at most
1,024 apps. It opens one immutable request per shard to that cluster's endpoint
and never mixes apps from two clusters in one response. All shards of one
snapshot share a new `registration_generation` and carry zero-based
`shard_index` plus `shard_count`; each shard's app set is complete for the
lifetime of its response:

```text
SubscribeRequest {
  wire_version,
  cluster_id,
  worker_id,
  term_permit: Bytes,
  registration_generation,
  shard_index,
  shard_count,
  apps: [{
    app_id,
    expected_binding: {
      database_id,
      database_epoch,
      grant_generation
    },
    cursor: AppCursor {
      app_id,
      worker_id,
      relay_id,
      leader_term,
      database_id,
      database_epoch,
      grant_generation,
      next_seq
    } | none
  }]
}
```

The request bytes are, in order,
`u16 wire_version || ClusterId || WorkerId || Bytes term_permit ||
u64 registration_generation || u32 shard_index || u32 shard_count ||
Vec<AppRegistration>`. A term permit is capped at 512 bytes. The decoder
requires `0 < shard_count` and `shard_index < shard_count`. An
`AppRegistration` is
`AppId || ExpectedBinding || Option<AppCursor>`; `ExpectedBinding` is
`DatabaseId || u64 database_epoch || u64 grant_generation`; and `AppCursor` is
`AppId || WorkerId || RelayId || u64 leader_term || DatabaseId ||
u64 database_epoch || u64 grant_generation || u64 next_seq`. These definitions
and the frame table are the complete v1 field order. The encoder does not use a
Rust enum layout, `bincode` defaults or architecture-sized integers.

The non-circular permit commitment is SHA-256 over domain
`zs-cdc-subscribe-v1\0` plus these canonical request bytes with `term_permit`
encoded as zero-length `Bytes`. Issuance and redemption use that same digest;
neither the permit itself nor the HTTP `Authorization` header is in its preimage.

The endpoint is operator-controlled topology, not a creator URL. The relay
rejects a request whose `cluster_id` differs from its configured cluster, then
authenticates the worker assertion and redeems the control-signed term permit as
specified in 6.2 before examining any app id. The permit and redemption must bind
the request's cluster, worker-process id, credential, admission epoch, relay pair,
shard key and commitment;
the authenticated assertion credential id must equal the permit's credential.
`worker_id` is a typed, UUIDv7 `wrk_` identity minted at process start and bound
into its supervisor-issued process credential; `relay_id` is the equivalent
`rly_` relay-process identity. Neither
survives a process restart. `registration_generation` starts at one and is monotonically
increasing per worker process **snapshot**, with all of that snapshot's sibling
shards sharing it. The relay keys shard admission by
`(worker_id, registration_generation, shard_index)` and requires one fixed
`shard_count` for the generation. "Identical request" means the binary body
bytes only; the `Authorization` header is excluded because every attempt must
carry a freshly minted single-use assertion. Admission compares the generation
before consulting an existing request key. A generation below the highest
admitted generation is always `NonMonotonicRegistrationGeneration` and does not
disturb any response, even when its body is byte-identical to an old key whose
response remains open. An identical-body retry is eligible only while its
generation is still the highest admitted, unpoisoned generation in the current
`(relay_id, leader_term)`. Admission state is term-scoped and is discarded on
self-demotion before the process can reacquire a higher term.

Before superseding a response, the relay revalidates every requested app
against the current topology, Datastore state, Grant and serving binding.
Revocation, rebind and reset fences cancel the affected response and invalidate
its cached accepted outcome. An exact retry may then produce current per-app
rejections from the same request body, but it can never reuse the old
authorization, relation primes or accepted cursor. If revalidation succeeds,
the retry atomically installs a new response token and cancels the prior
response whether or not that server task already emitted `Registered`. This
covers a terminal frame lost in transport and ensures two writers never serve
one admission. The relay reconstructs primes and the terminal outcome from the
request's captured cursor plus current authoritative state; it does not reuse
connection-local batch ids from the cancelled response.

A distinct shard index is admitted once. Different body bytes or a different
`shard_count` under an admitted generation are `ConflictingShard`. An app
repeated within one request or across sibling shards is `DuplicateApp`. Either
error poisons the whole candidate generation: the offending request gets `400`,
every already
open response for that generation is cancelled, and the worker marks any app
that had already selected it reconnecting. It surfaces a local invariant and
does not mint another generation until the active-set snapshot or process
version changes; blindly regenerating the same malformed partition would be a
hot loop. The relay retains a term-scoped poison tombstone with the original
closed code. An identical or different retry of that generation returns the
same code and cannot reinstall a response; only a higher generation or a new
leader term retires the tombstone.
A transport retry before the worker has processed `Registered` keeps the same
generation and byte-identical body. Once the worker processes `Registered`,
any app-set, binding or cursor change mints a new generation and replaces every
shard.
`AppCursor` is the next per-app semantic sequence the worker needs, not the last
global LSN it observed. Every retained semantic frame has one strictly
increasing `seq` in its binding ring and the worker advances `next_seq` only
after the local broker has processed that frame. Cursors are memory-only; a
worker restart sends none and receives a reset. `grant_generation` is a
control-plane monotonic counter incremented on every Grant activation, revoke or
rebind, including a move away from and back to the same Database. Database epoch
alone cannot identify a binding: two Databases may both be at epoch one.

The service assertion authenticates a trusted worker process, not creator code.
The fleet is deliberately one worker trust domain: any worker may be selected
by CHWBL to host any app, and the same process already receives that app's
bundle, environment and Database binding. The accepted cost is fleet-wide
impact from compromise of a worker process; inventing an assignment service
only for CDC would not narrow the rest of that existing authority. The native
`cdc_lifecycle` code builds `apps` solely from its server-injected active lease
map. No V8 value, request field or creator header can supply or replace an
`app_id`, binding or cursor, and the relay independently requires the current
active Grant before returning rows. A native-boundary test passes hostile app
ids and proves none reaches `SubscribeRequest`.

**Resume decision and acknowledgment.** For each requested app the relay makes
exactly one decision in this order; a later rule never hides an earlier
failure:

1. Reject an app absent from topology, without an active Grant, or whose
   Datastore stream is unavailable.
2. Compare the expected Database and Grant generation with the worker-visible
   serving binding. A mismatch is `StaleExpectedBinding` and requires the
   worker to re-resolve control state; it is not converted into a reset.
3. If desired, observed and serving Database epochs are not all equal, reject
   as `EpochPending` until section 8's marker/topology rendezvous completes.
   This check precedes expected-epoch comparison, so a worker correctly
   carrying the still-serving E receives `EpochPending` while desired E+1 is
   hidden behind the barrier.
4. Compare the expected epoch with the serving epoch. A mismatch is
   `StaleExpectedBinding`.
5. If a cursor is present, require its `app_id` and `worker_id` to equal the
   request. A cross-app or
   cross-worker cursor is `CursorBindingMismatch`.
6. No cursor resets as `Initial`.
7. A cursor Database or Grant generation mismatch resets as `GrantRebound`; an
   epoch mismatch resets as `DatabaseEpochChanged`. Binding comparison comes
   before term comparison so a simultaneous rebind and failover cannot retain a
   watermark from the old Database.
8. A different `relay_id` or `leader_term` resets as `RelayFailover`.
9. Finally, a `next_seq` above `ring_next_seq` is rejected as `CursorAhead`, one
   below `tail_seq` resets as `RingOverrun`, and an in-range cursor resumes.

The retained interval is `[tail_seq, ring_next_seq)`; for an empty ring,
`tail_seq = ring_next_seq`. Replay sends every frame with `seq >= next_seq`,
and equality with `ring_next_seq` means caught up. Every reset skips the
retained interval for that connection and accepts exactly the current
`ring_next_seq`; replay begins with the next live sequenced frame. One per-app
rejection does not close accepted apps.

Authentication failure is an HTTP `401`. `UnsupportedWireVersion`,
`ClusterMismatch`, `DuplicateApp` and `NonMonotonicRegistrationGeneration` are
request-level protocol errors and close the response without `Registered`. The
complete per-app `RegistrationRejectCode` enum is `AppNotInTopology`,
`GrantInactive`, `DatastoreUnavailable`, `StaleExpectedBinding`,
`EpochPending`, `CursorBindingMismatch`, `CursorAhead` and
no other code. Noncanonical ids, zero or out-of-domain generations, invalid
option tags and truncated cursors fail request decoding as `MalformedRequest`;
they are not a partly decoded per-app outcome. There is no free-form error
string on the wire.

`EpochPending` and `DatastoreUnavailable` are retryable. The worker keeps the
accepted apps on the old response, backs off from 250 milliseconds
exponentially to five seconds, refreshes topology, and opens a make-before-break
replacement request with current cursors until the app is accepted.
`StaleExpectedBinding` triggers one immediate topology refresh before entering
the same backoff. `AppNotInTopology` and `GrantInactive` retry only after the
server-injected active lease set changes; `CursorAhead` and
`CursorBindingMismatch` are local invariant failures and do not spin.

Before acknowledging an accepted app, the relay emits `RelationPrime` for every
relation generation referenced by the replay interval **and every currently
active relation generation**. Relation descriptors remain retained while a ring
frame references them, and the current descriptor remains until superseded.
Each registration app gets a connection-monotonic, never-reused
`prime_batch_id`. After all primes for those batches, the relay emits one
terminal `Registered` frame for the registration generation. Each outcome names
its `app_id` and is either
`Resumed { prime_batch_id, prime_relation_count, database_id, database_epoch,
grant_generation, accepted_cursor }`,
`Reset { prime_batch_id, prime_relation_count, database_id, database_epoch,
grant_generation, reason, accepted_cursor }`, or `Rejected { code }`, in
exactly request order. An
`accepted_cursor` is the complete current `AppCursor`, including `app_id`,
`worker_id`, relay, term and binding identities. For resume its `next_seq` is
the requested value; for reset it is the `ring_next_seq` captured with the
primes. No app data frame is sent before `Registered`.

The worker stages every `RelationPrime` by
`(response_instance, app_id, prime_batch_id)` and does not expose it to the
decoder yet. `response_instance` is local connection identity, not a wire
field; it prevents a delayed prime from another shard or retry from satisfying
the batch. The terminal count makes completeness explicit:
exactly `prime_relation_count` distinct `relation_generation` values must have
arrived for the batch. A second descriptor for one generation is a duplicate
even if its bytes are identical.
Count zero defines a complete empty batch and is valid even though no preceding
`RelationPrime` introduced the id. On `Registered::Resumed` it atomically replaces the
relation cache with that staged complete batch and starts replay. On
`Registered::Reset` it first clears old relation and live-query state, installs
the staged batch atomically, applies 6.4's numeric-watermark rule, publishes one
local broker `Resync`, and only then accepts semantic data at
`accepted_cursor`. Thus reset cannot discard the primes sent immediately before
it. A count mismatch, duplicate prime, or already-consumed batch id is fatal;
an otherwise unknown id is fatal only when its declared count is nonzero. The
identity checks above run before this batch is consumed.

If an already-registered cursor later falls behind, the relay clears that app's
queued egress frames on that connection, emits a fresh staged prime batch,
emits the unsequenced connection-local `Resync` naming that batch and a freshly
captured complete `accepted_cursor` at `ring_next_seq`, and then queues semantic
data. The worker applies the same clear, atomic install, local reset and cursor
transition in response order. There is no reverse acknowledgment channel. A
future relation generation is introduced by a sequenced `Relation` before the
first `Change` that references it. This makes registration success, binding
identity, resume coordinates and relation-cache readiness observable rather
than timing assumptions.

An app-wide reset has one exact cursor transition. The relay appends
`AppReset` at sequence S before activating reset coverage. A connection whose
egress is still valid consumes that shared frame and advances to S+1. A barrier
that must clear queued egress instead sends that connection a prime batch plus
connection-local `Resync` with `accepted_cursor.next_seq = S+1`; it removes the
shared reset from that connection's queue, so the worker never processes both.
If the control cannot be queued, the response closes. Ring compaction may set
`tail_seq = S` but never decreases `ring_next_seq`. No cursor skips S without
one of these two observable reset paths.

**Lease changes are make-before-break within one cluster.** `cdc_lifecycle`
gains the active-set snapshot accessor described in section 4. The worker opens
every shard of a new generation with current cursors, but there is no global
all-shards activation barrier. Each shard may stream immediately after its own
`Registered`, so the worker validates that whole terminal frame and its staged
primes, applies every required reset, then atomically selects the new response
for each accepted app in that shard. A rejected app keeps its old selected
response. Every data frame is fenced against the app's selected
`(registration_generation, shard_index, response_instance)` before broker
delivery; a late old-response frame for an already switched app is discarded
even if its `seq` would otherwise pass. The worker closes an old response only
when no app still selects it. It may therefore keep reading one response for a
rejected app while dropping that response's frames for already switched peers.

A newly needed cluster gets its own shard set; an empty cluster set closes only
that set. This per-app switch avoids unbounded buffering while a sibling shard
is slow or rejected. If the new generation receives
`ConnectionCapacityExceeded` because old responses hold the required egress
permits, the worker closes the minimum old responses needed, marks every app
that selected them reconnecting, and retries; this is the one explicit
break-before-make availability fallback. A response is never mutated in place,
and the protocol has no hidden client-to-server update channel. Sorting and
chunking can move many apps when one id is inserted; the extra reconnect churn
and per-app selection map are accepted instead of a process-wide 1,024-app CDC
ceiling.

`ensure_ready` currently waits only for a locally spawned consumer
(`crates/zeroship-plugin-db/src/cdc_lifecycle.rs:211-216`). It now completes
for one app only after that app's relation priming, `Registered` outcome and
any required `Resync` have been processed by the broker. A slow sibling shard
is not part of its readiness. That preserves the initial snapshot boundary.
`Heartbeat.last_confirmed_lsn` is Datastore health telemetry only and is never a
worker resume coordinate.

### 5.3 Delete, and "row left the view", without a full before-image

Section 3.5(b) rules out `REPLICA IDENTITY FULL`. So a DELETE frame carries the
replica-identity columns and the pk, and pgoutput supplies an UPDATE old tuple
only when the replica-identity columns changed (this is pgoutput's own contract,
documented at `libs/compio-postgres/src/replication.rs:1930-1932`). The relay
uses that tuple only to recover the identity when required, then discards it.
Wire `Change` and the target broker `ChangeEvent` carry no old tuple.

The broker's current answer is to test the predicate against both tuples
(`broker.rs:293-297`). Without a before-image that arm cannot fire, and a row
that leaves a subscriber's view would silently stop updating.

The replacement: the subscription keeps the bounded set of pks it has delivered
into the subscriber's current view. An event whose pk is in that set is delivered
regardless of whether the new tuple matches the predicate, so the subscriber
learns the row left. For an identity-changing UPDATE it removes that old `pk`
and evaluates the new identity derived from positional values for insertion.
The set is bounded by the query's page size; overflow emits `Resync`, which the
broker already models (`broker.rs:318-340`) and the live-query client absorbs
(7.3).

This is **not prototyped**, and it is the one part of the design where the
subscriber-side contract changes shape rather than moving (12.4, 13).

### 5.4 Slow consumers: the relay sheds, it never blocks

**The invariant, and everything else in this subsection is a consequence of it: a
consumer must never be able to reach the slot.** The ring writer advances
`highest_gap_covered_lsn`, and that bound is what confirms the slot (6.3). The
name is deliberate: volatile ring admission closes the current term's gap but
does not make the event durable across a relay crash. A ring writer that can be
blocked by a subscriber is a subscriber
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
- **A cursor that falls behind the ring's tail gets the connection-local
  `Resync(RingOverrun)` procedure from 5.2 and jumps to `ring_next_seq`.** No
  shared sequence is consumed and healthy workers are untouched. It is not
  disconnected. Disconnecting makes the worker re-register and re-lease, which
  is more work under load, not less.
- **A connection whose egress buffer has been full for longer than
  `slow_consumer_timeout` is CLOSED.** The worker reconnects and receives
  the normal registration decision: `Resumed` if its cursor is still retained,
  otherwise `Registered::Reset(RingOverrun)`. Closing beats holding: a socket
  the peer is not reading is relay memory the ring needs. A full socket does not
  itself prove that the ring cursor was overrun.
- **No credit scheme, and this is a decision rather than an omission.** Credit is
  flow control, and flow control on this feed means a consumer can slow the
  producer, which is the one thing the invariant forbids. The consumer's only
  legitimate signal is "I fell behind", and `Resync` already carries it.

There is one honest cost. Under this policy a worker that is merely slow rather
than wedged reconnects repeatedly and, once the ring overtakes it, gets
`Resync` storms; on the live-query path each `Resync` is a full refetch (7.3).
A slow subscriber is therefore converted into connection churn and eventually
database read load. That is the accepted trade: read load is bounded by the
app's own spend limit and WAL growth is not.

### 5.5 Ring sizing

Ten numbers are operator configuration with these mandatory defaults:

| knob | default | bound and reason |
| --- | --- | --- |
| `ring_bytes_per_app` | 8 MiB | At least `2 * max_frame_bytes` and no more than `ring_bytes_total`. This is the primary per-tenant bound |
| `ring_frames_per_app` | 16,384 | At least 2. It bounds tiny-frame index overhead inside the byte budget |
| `ring_retention` | 60 seconds | At least 1 second. It decides how long a disconnected worker can resume |
| `ring_bytes_total` | smaller of 512 MiB or 20% of the detected cgroup memory limit | At least `ring_bytes_per_app`. It bounds aggregate ring RSS |
| `egress_bytes_per_connection` | 2 MiB | At least `max_frame_bytes + 4` plus queue metadata, and included in the process memory admission calculation |
| `egress_bytes_total` | smaller of 128 MiB or 5% of the detected cgroup memory limit | At least `egress_bytes_per_connection`. It is a global byte semaphore, not an observed-connection estimate |
| `slow_consumer_timeout` | 15 seconds | At least 1 second. It bounds how long a full egress queue remains connected |
| `app_fault_threshold` | 5 faults | At least 2. The fifth app-data projection/encoding fault inside the rolling window enters quarantine |
| `app_fault_window` | 60 seconds | At least 10 seconds. Faults older than the window do not count |
| `quarantine_duration` | 300 seconds | At least one storm window. Reactivation is attempted once after this interval |

Startup requires
`ring_bytes_total + egress_bytes_total <= 25%` of the detected cgroup memory
limit. Before sending `200`, a subscription must acquire one full
`egress_bytes_per_connection` permit from the global egress semaphore; if none
is available it gets `503 ConnectionCapacityExceeded` and no queue is
allocated. The permit is held through response close. Consequently the hard
concurrent-response ceiling is
`floor(egress_bytes_total / egress_bytes_per_connection)` even though startup
has no live connections to count. The accepted cost is retrying otherwise valid
workers at that ceiling instead of letting connection fan-out defeat the RSS
budget.

These are provisional launch defaults, chosen to bound failure rather than
claimed as workload measurements. Section 11's measured p99 frame size,
per-app burst bytes, worker-restart duration and concurrent subscriber count
are the four facts that may change them; until that measurement exists,
implementations use these values rather than inventing their own.

Eviction is always oldest-first within one app's ring and increments that app's
overrun counter; each cursor below the new tail is reset independently. **There
is no drop-the-newest arm**: the newest frame is
the one a live query most needs, and a policy that discards it converts a burst
into a permanently stale view rather than a refetch.

The retention windows of durable-log products - Salesforce CDC 72 hours, Kinesis
and DynamoDB Streams 24 hours by default, Neon's 40-hour slot reaper, all
**unverified** and relayed from the prior-art study - are **not** targets. Their
retention *is* the product. This ring is a fan-out buffer in front of a slot that
retains WAL only until the relay confirms it. After confirmation there is no
durable subscriber log; a relay-term change signals that gap with `Resync`. The
right first value is the one that makes `Resync` rare for a worker restarting
normally and never lets one tenant's burst evict another's, which is derived from
section 11's worker-restart and per-app-write-rate measurements.

What forbids simply making the ring large: RSS is the resource one tenant can
exhaust for all of them, and 7.5 exists because of that. A bigger ring buys
disconnect tolerance and sells fault isolation.

### 5.6 The vocabulary for what the relay cannot represent

The worker broker's `Resync` is a total reset for that worker's app:
`resume_app_with_resync` pushes it to every local subscription
(`broker.rs:724-746`). A connection-local relay `Resync` maps to that operation
for one worker; a sequenced `AppReset` maps to it on every worker following the
shared app ring. Both carry the same closed `ResetReason` from 5.2. This is
load-bearing: when a barrier clears queued `AppReset(S)`, the replacement
connection-local `Resync(S)` preserves the original reason and therefore the
same watermark decision. These resets are the right vocabulary for a lost
interval. They are wrong for the three conditions below, each of which needs a
narrower vocabulary. The prior art made all three first-class: Salesforce has
`GAP_UPDATE` and `GAP_OVERFLOW` as distinct event types, and Debezium ships a configurable
`unavailable.value.placeholder`.

1. **One change for one row that cannot be represented.** A value over the frame
   budget or a type the wire format has no encoding for. Resyncing a whole app
   for one bad row is a refetch storm.
2. **A value the server did not send.** pgoutput sends an unchanged-TOAST marker
   in place of a large unmodified value. The relay must not guess and must not
   encode it as NULL.
3. **A truncate.** `PgOutputMessage::Truncate`
   (`libs/compio-postgres/src/replication.rs:1948-1955`) names relation ids and
   carries no rows. It is not a `Change` and it is not a `Resync`; it is a fact
   about every row of a collection.

So, beside `Resync` and `AppReset`:

| frame | payload | meaning |
| --- | --- | --- |
| `Gap` | `(collection, pk, commit_lsn, change_index, reason)` plus the common app envelope | one change for one row could not be represented; that row's current state is unknown, everything else is intact |
| `Truncate` | `(collections)` plus the common app envelope | every row of these collections is gone |

and inside `Change`, `CellValue` is `Value(Bytes) | Null | Unavailable` rather
than `Option<Bytes>`. **Encoding "the server did not send it" the same way as "it
is NULL" is the same class of collapse section 3 exists to prevent**: a
downstream consumer that cannot tell them apart will eventually write the wrong
one into a cache and call it a value.

`Gap.reason` is a **closed** enum - `OversizeValue`, `UnrepresentableType`.
Closed rather than a string, because an open reason field is
somewhere a physical column name reappears on the wire the moment someone writes
a helpful error message. If even the keyed `Gap` exceeds `max_frame_bytes`, the
relay appends `AppReset(AppDegraded)` and enters the active-reset quarantine of
7.5 before confirming the change. It does not kill the Datastore stream or emit
a truncated key.

The subscriber contract absorbs both new frames for free on the path that
matters: `sdks/db/src/live.ts:333-338` reruns on `change` and `resync` alike, so
`Gap` and `Truncate` join that arm and cost one refetch. On the raw
`db.subscribe` path they are two more `kind` values a creator can ignore, which
is 7.3's existing honest problem made two cases wider - argued in 12.9.

---

## 6. Leader election and resume, without tokio

### 6.1 One cluster leader owns one stream per Datastore

The current deployment happens to have one physical database, but the target
data model does not. `Datastore` is one physical PostgreSQL database;
`Database` is one creator-owned schema within it; `Grant` binds an app to one
Database. The designed entity shape is recorded at
`docs/proposals/2026-08-28-app-database-decoupling.md:207-215`. These entities
do not ship yet, so the relay sequences behind that control-plane work.

A logical slot's namespace and quota are physical-cluster-scoped, while its
decode is bound to exactly one Datastore. Therefore:

- election cardinality is one leader per stable `cluster_id`;
- replication cardinality under that leader is one exact slot and one stream per
  Datastore in the cluster;
- each stream names only the shared publication in its own Datastore;
- a second Datastore adds a stream; it cannot be folded into an existing slot.

The relay refuses a topology snapshot that maps one Datastore to two clusters,
duplicates a slot name, or lacks the reserved slot/walsender capacity described
in 3.6. This makes the ten-Datastore stock ceiling a provisioning refusal
rather than an outage discovered by the eleventh tenant.

### 6.2 Election

PostgreSQL advisory locks are database-scoped. Every contender therefore takes
the lock in one designated coordination database, not in whichever Datastore it
happens to open.

**Coordination endpoint.** The database is named `zeroship` on the platform
control-plane PostgreSQL deployment. Relays receive a dedicated
`ZEROSHIP_CDC_COORDINATION_DATABASE_URL`. The platform migration creates
`control.cdc_clusters` and the `zeroship_cdc_coord` login, then grants exactly:

```sql
GRANT CONNECT ON DATABASE zeroship TO zeroship_cdc_coord;
GRANT USAGE ON SCHEMA control TO zeroship_cdc_coord;
GRANT SELECT (
  cluster_id, advisory_lock_key, postgres_system_identifier,
  postgres_timeline, leader_term, leader_relay_id, predecessor_fenced_term,
  topology_revision
) ON control.cdc_clusters TO zeroship_cdc_coord;
GRANT UPDATE (leader_term, leader_relay_id)
  ON control.cdc_clusters TO zeroship_cdc_coord;
GRANT USAGE ON SCHEMA service_authn TO zeroship_cdc_coord;
GRANT SELECT, INSERT, UPDATE, DELETE
  ON service_authn.service_assertion_replay TO zeroship_cdc_coord;
REVOKE CREATE ON SCHEMA service_authn FROM zeroship_cdc_coord;
```

The table grants are the exact privileges required by the existing atomic
claim-and-expiry-sweep implementation
(`crates/zeroship-authn/src/service_replay.rs:47-58`). The role receives no
other control-plane, Datastore or creator-schema privilege.
Replication and management DSNs are separate per-Datastore secrets. Reusing
any of those DSNs for election is forbidden.

The control plane adds a typed `ClusterId` (`cls_<base62 uuidv7>`) and one
registry row per physical PostgreSQL cluster:

```text
cdc_cluster {
  cluster_id primary key,
  advisory_lock_key int4 unique not null,
  postgres_system_identifier numeric(20, 0) unique not null,
  postgres_timeline bigint not null
    check (postgres_timeline > 0 and postgres_timeline <= 4294967295),
  leader_term bigint not null check (leader_term > 0),
  leader_relay_id text,
  predecessor_fenced_term bigint not null
    check (predecessor_fenced_term > 0
       and predecessor_fenced_term <= leader_term),
  topology_revision bigint not null
}
```

Provisioning seeds `leader_term` and `predecessor_fenced_term` to the same
positive, non-serving value and leaves `leader_relay_id` null. The first leader
increments the term before serving.

Provisioning obtains `(postgres_system_identifier, postgres_timeline)` from
`IDENTIFY_SYSTEM` over the first Datastore's replication connection. Every
later Datastore DSN must return that exact pair before it can join the cluster
or become CDC-ready. The driver `u32` timeline converts losslessly to the
checked `bigint` and back with a fallible typed conversion; `int4` is rejected
because it cannot hold timelines above `i32::MAX`. The
unique constraint prevents two `cluster_id` values naming one physical cluster;
the per-Datastore check prevents one `cluster_id` spanning two physical
clusters. A mismatch is a hard configuration refusal, never a warning.

`CDC_LOCK_NAMESPACE` is the fixed signed-int4 value `1_515_406_148`
(ASCII "ZSCD"). The second integer key is allocated and uniqueness-constrained; hashing
`cluster_id` into a lock key is rejected because a collision would make two
clusters exclude each other. Every relay instance is configured with exactly
one `cluster_id`. On a dedicated coordination connection it calls
`pg_try_advisory_lock(CDC_LOCK_NAMESPACE, advisory_lock_key)`. After success,
on the same session, it atomically increments `leader_term`, writes its
`leader_relay_id` and uses the returned pair in `Hello`. The update predicate
requires `predecessor_fenced_term = leader_term` as well as
`leader_term < 9223372036854775807`; a standby cannot skip an unfinished fence.
An operator-owned `SECURITY DEFINER` row trigger has a no-login owner, fixed
`search_path = pg_catalog, control`, and `PUBLIC` execute revoked. It freezes every active old-term member
against the new relay pair with a random challenge in that transaction. It alone sets
`predecessor_fenced_term` to the new term when the set is empty. The relay role
cannot invoke it directly or edit fence state. Exhaustion is a hard operator
refusal rather than an overflowing increment. It opens no Datastore stream
before that commit. Lock or predicate misses retry with bounded jitter and serve
no worker stream.

`cdc_endpoint` is a cluster-local load-balanced service address. A contender's
readiness endpoint is green only while its leadership guard and topology loop
are live, so the load balancer routes subscriptions only to the leader.
Datastore readiness is reported separately and a failed Datastore produces
`Rejected(DatastoreUnavailable)` only for its apps; healthy
Datastores on the same response remain accepted. Making global readiness depend
on every stream is rejected because one broken tenant database would remove the
only lock holder from the load balancer while preventing a standby from taking
over. A non-leader answers a direct subscription attempt with `503 NotLeader`
and no redirect URL; the worker retries the operator-provided endpoint with
bounded jitter.

**The connection probe accelerates self-demotion; the predecessor ACK is the
delivery fence.** The dedicated coordination session lives for the entire
leadership term and is used only for the lock, term update and a
`SELECT leader_term, leader_relay_id ... WHERE cluster_id = $1` probe every two
seconds. Each probe has a three-second deadline. EOF, timeout, query error,
ambiguous cancellation, a missing row, or a returned pair different from the
held pair atomically trips one leadership cancellation token. The token is
checked immediately before every confirmation, ring write, topology
acknowledgment and worker-frame send; tripping it cancels every Datastore task
and response. This active probe, not TCP optimism, detects a half-open path. The
process must reacquire the lock and mint a higher term before serving again.
The central control database is therefore an accepted CDC availability
dependency; uncertainty self-demotes rather than risking split brain.

Control owns one durable `CdcWorkerTermMember` per `(cluster_id, worker_id)` with
credential id, monotonic nonzero `admission_epoch`, active/fenced terms, pending
predecessor/successor pair and challenge, and completed receipt. It owns
`CdcWorkerGrantFence` rows keyed by `(cluster_id, worker_id, revision, app_id,
old_grant_generation)`, each with challenge and worker-or-supervisor receipt.
Typed cluster routes cover permits, releases, redemptions and both fence ACKs.
The first request fixes the credential; later routes require it and mismatches
change nothing. A new process receives a new worker id.

Workers poll `term-permits` every two seconds and per shard with nonce, request
commitment and `expected_admission_epoch`; `null` creates only an absent row at
1. Every later worker-originated membership mutation locks cluster before member
and compares that epoch. Mismatch is side-effect-free `StaleAdmissionEpoch`. For
release or fence completion, exact durable receipt lookup precedes the comparison,
so replay returns its recorded epoch without reapplying. Control then
activates the member in the current term and returns a signed current-pair permit
bound to that epoch, or returns `FenceRequired { term?, grants[] }`.

A PendingRevoke/Rebind snapshots all active members into Grant-fence rows;
inactive workers cannot hold valid responses, and permit issuance/redemption is
blocked while the transition is pending. Online redemption requires the current
relay pair, unchanged member epoch, no pending Grant transition, and an exact body for first-claim
replay. Issue, redemption, release, transition creation, fence completion and
term increment all lock cluster before member/fence. A releasing worker holds
its exclusive guard while cancelling and joining cluster response/admission
tasks, purging queues and calling control. A preexisting fence returns its exact
`FenceRequired` without clearing active; the worker completes it under that
guard and retries. Accepted release clears active and increments
`admission_epoch` before guard release; operation nonce and request hash key its
receipt. Thus transition-first fences the member, while release-first removes it
and invalidates earlier permits before the transition snapshot.

N+1 freezes every active N member against its exact successor and challenge. For
either fence set, the worker takes one cluster-exclusive guard, drains delivery,
cancels named responses, purges queues, records per-app minimum Grant generations
and sends exact ACKs. Term ACK echoes worker, both relay pairs and challenge;
Grant ACK echoes its row key and challenge. Term completion atomically receipts
all incomplete Grant fences visible for that worker, clears active/pending,
advances fenced-through and stores its receipt; the last member advances
`predecessor_fenced_term`. Term-first makes the member absent from a later Grant
snapshot; Grant-only completion leaves it active. The first worker or supervisor
batch increments `admission_epoch` once. Exact challenge receipts replay without
another increment; missing or newly visible rows return the complete
`FenceRequired` set without mutation.

Control exposes revoke/rebind only after all frozen-worker receipts and an exact
current-pair relay topology ACK. A term change invalidates older relay evidence,
so the successor must ACK even when worker receipts exist. Only the supervisor
may complete either fence after exact process death and credential revocation,
using its termination record. There is no timeout: a paused worker blocks until
it drains or is externally fenced, even without N+1 `Hello`.

Each Datastore topology entry names a separate
`management_dsn_secret_ref`. Its login may read `pg_replication_slots` and
`pg_stat_activity`, read only the five head columns granted in 3.7, update only
`__zeroship_cdc.heartbeat`, and is a member of `pg_signal_backend`; it has no
creator-schema DML privilege or head write privilege. After acquisition,
the leader may terminate an old `active_pid` over that connection only after one
query proves the slot name equals the exact Datastore-derived name,
`database = current_database()`, `slot_type = 'logical'`, the plugin is
`pgoutput`, and the backend role and `application_name` equal the provisioned
relay replication identity. Failure of any predicate refuses termination. This
is privileged process authority and is deliberately kept out of the worker.

A worker that accepts term N+1 closes and ignores every lower-term response; its
fenced acknowledgment proves that state even when N+1's `Hello` was withheld.

**Topology channel.** `ZEROSHIP_CONTROL_URL` is required and must be HTTPS. The
leader mints a single-use service assertion for exact audience
`spiffe://zeroship.ai/svc/control` and calls
`GET /internal/v1/cdc/clusters/{cluster_id}/topology?after_revision=R`. The
control plane long-polls for at most 25 seconds and returns either no change or
one complete typed snapshot; it never returns a patch. The relay validates the
whole snapshot and its foreign keys before atomically replacing revision `R`.
It reconnects immediately after a response, so both a new Database and a Grant
change reach the relay without a stream restart.

```text
Cluster {
  cluster_id,
  cdc_endpoint,
  postgres_system_identifier,
  postgres_timeline
}
Datastore {
  datastore_id,
  cluster_id,
  replication_dsn_secret_ref,
  management_dsn_secret_ref,
  reset_generation,
  cdc_state:
    Ready
    | Resetting {
        phase: Quiesce | Restore,
        kind: ClassificationChanged | SlotInvalidated | SystemIdentityChanged
      }
}
Database {
  database_id,
  datastore_id,
  physical_schema,
  desired_database_epoch,
  serving_database_epoch
}
Grant {
  app_id primary key,
  database_id,
  grant_generation,
  state: PendingActivate | Active | PendingRevoke | Revoked
}
UNIQUE (datastore_id, physical_schema)
```

The response carries `revision` outside those entities. After applying it, the
leader calls
`POST /internal/v1/cdc/clusters/{cluster_id}/topology-acks` with a fresh
service assertion and this closed body:

```text
TopologyAck {
  revision,
  relay_id,
  leader_term,
  datastore_statuses: [{
    datastore_id,
    reset_generation,
    state: Ready | Quiesced | RestoreReady | Unavailable,
    database_epochs: [{
      database_id,
      observed_database_epoch
    }],
    grants: [{
      app_id,
      database_id,
      grant_generation,
      state: PendingActivate | Active | PendingRevoke | Revoked
    }]
  }]
}
```

`reset_generation` is a nonzero monotonic counter seeded at Datastore
provisioning and incremented by every reset barrier. Its `kind` is immutable
until that generation reaches `Ready`: `ClassificationChanged` completes only
through migration finalize; the other two kinds complete only through
`ResetHeads`. The status vector contains
every Datastore in revision `R` exactly once, sorted by typed id. `Ready` and
`RestoreReady` contain every Database and Grant for that Datastore exactly
once, also sorted, with no foreign child or duplicate. `Quiesced` and
`Unavailable` require both child vectors empty because neither has a live
decoder; `Quiesced` attests that exclusive acquisition drained earlier writers
and proved the exact slot absent, while durable reset state bars later unrelated
writes; `RestoreReady` additionally attests the relay currently holds exclusive.
`Unavailable` never satisfies a
barrier. A topology `Ready` entry
admits `Ready` or `Unavailable`,
`Resetting { phase: Quiesce, .. }` admits `Quiesced` or `Unavailable`, and
`Resetting { phase: Restore, .. }` admits `RestoreReady` or `Unavailable` at the
exact `reset_generation`. Unknown enum values, a missing status, inconsistent
reset generations, a non-`Unavailable` phase/status mismatch or a child set
that is not the exact topology slice rejects the whole ACK.

Control accepts the acknowledgment in one transaction that row-locks the same
`control.cdc_clusters` row used by term increment. Under that lock it requires
the exact `(leader_relay_id, leader_term)`,
`predecessor_fenced_term == leader_term`, and `revision` no greater than the
latest topology revision; it writes all resulting barrier state before
unlocking. Thus either an N+2 term update precedes the check and rejects N+1, or
the N+1 ACK commits before N+2 can become current. A stale-term, unfenced or
future-revision acknowledgment is rejected; an exact replay is idempotent. ACKs
are cumulative complete-snapshot evidence, not one global "current pending"
latch. For each Datastore status, control stores the greatest acknowledged
revision, its relay pair, and the exact observed reset, Grant and Database-epoch generations. An
ACK at R may satisfy a barrier created at B only when `R >= B`. A prepare-reset
barrier requires `Quiesced` at its exact
reset generation under a `Quiesce` snapshot. Finalize first publishes `Restore`
and then requires `RestoreReady` at that same generation; accepting that ACK is
the transaction that publishes `Ready`. A Grant barrier requires its exact
generation and state, and an epoch barrier requires observed epoch equal to
desired; a larger unrelated counter is never guessed to mean the same
transition.

Barriers serialize per Datastore, not per cluster. A revision R1 waiting on
failed Datastore A can coexist with R2 for healthy B; B's R2 status completes
its barrier while A's remains pending. A Grant or epoch barrier completes only
when every Datastore it names has qualifying cumulative evidence. A
cluster-identity barrier deliberately names all Datastores. Each accepted
per-barrier acknowledgment is durable before control exposes that transition
to workers. The cost is a per-Datastore acknowledged-state row rather than one
cluster-wide pending bit.

`replication_dsn_secret_ref` and `management_dsn_secret_ref` are basenames in
the required `ZEROSHIP_CDC_DATASTORE_SECRETS_DIR` mount. The relay opens them
relative to a pre-opened directory descriptor with no symlink following,
requires owner-only file permissions, and parses exactly one PostgreSQL URL per
file. A topology revision rotates a credential by naming a new file; files are
never rewritten in place. A missing, malformed or over-permissive secret makes
only that Datastore `Unavailable` and is reported in the fenced topology
acknowledgment. There is no implicit environment-name convention or network
secret fetch for an implementer to choose.

Every foreign-key axis shown above is mandatory. `grant_generation` increments
on every activation, revocation and rebind and never resets for an app. The
Datastore's `reset_generation` is likewise monotonic and supplies 3.6's durable
classification-barrier token. The immutable kind selects exactly one
completion endpoint; neither endpoint may reinterpret another kind. A relay
never opens a slot in `Quiesce`. It may
open only a fresh exact slot in `Restore` at that generation after the complete
head snapshot is durable, and it never
serves worker rows until control publishes `Ready`. The
mapping is authoritative. The app ids and expected bindings in a worker
registration only select and compare an allowed subset; they never decide a
schema, Datastore, endpoint or DSN. For every pgoutput relation the relay resolves
`(datastore_id, physical_schema) -> database_id -> active Grant -> app_id` and
fans the projected frame to every active grantee. All fan-out copies retain the
same `(commit_lsn, change_index)` and receive their own app sequence.

Grant activation, revocation and rebind are revision barriers. For activation,
the relay installs the pending route before acknowledging; only then does
control expose it as `Active`. For revocation or rebind, the relay first
generation-fences the app, clears its old ring and every queued egress frame,
and closes every immutable response containing the old binding before its ACK.
In parallel, 6.2's Grant fence makes each frozen worker drain delivery, purge
old-generation decoder/broker/subscriber queues and install its minimum generation
before ACK. Control exposes the transition only after both evidence sets; a new
or reactivating worker receives no permit while it is pending. Thus no buffered
old-Database row can cross broker admission or subscriber delivery after the
accepted barrier, in either the current or a predecessor term. The worker then
opens a replacement from its new active set. This can reconnect
otherwise healthy apps that shared a response with the changed app; that
availability cost buys a simple, testable confidentiality fence.

**Desired, serving and observed epoch are distinct.** Relay topology supplies
both control's desired epoch and its durable worker-visible serving epoch. On
an ordinary retained-slot path, the stream supplies observed epoch through
section 8's transactional marker; new-term initialization may seed it from
equal desired and serving after the fenced heartbeat below, and a destructive
reset may seed it only from the durable complete-head snapshot.
New registration requires all three to agree. An existing feed may finish
frames through the epoch for which observed equals serving even when desired has
advanced. It stops at the marker and cannot deliver an E+1 frame until all three
equal E+1. Thus topology-first gives new registrations `EpochPending` while
existing connections finish epoch-N frames; marker-first pauses at the marker.

Matching desired and observed is not yet permission to confirm the marker.
The relay keeps `highest_gap_covered_lsn` below that marker's commit, posts the
fenced topology ACK, and waits until control durably promotes serving epoch and
returns success. It then atomically appends one sequenced `Epoch` fence to every
active grantee app ring for that Database, marks
the local Database serving, unpauses delivery, and only afterwards admits or
confirms the marker commit. A crash before the ACK commit therefore replays the
marker; a crash after it may resume beyond the marker because control's serving
epoch is durable. A registration in the short control-promoted/local-not-yet-
serving interval still receives `EpochPending`. The worker processes all
earlier frames, clears relation and live-query state, re-resolves the binding,
publishes local `Resync`, and only then accepts frames after `Epoch`.

On a new leader term, the relay does not expose backlog from the old term after
workers refetch. For each topology-`Ready` Datastore with a retained slot, it
validates one snapshot revision R, activates reset coverage and drains through a
decoded heartbeat commit. For each Database, if that same snapshot has
`desired_database_epoch == serving_database_epoch` and the drain saw no newer
marker, the relay seeds in-memory observed epoch from serving: the prior fenced
ACK is durable evidence even when its marker is before
`confirmed_flush_lsn`. If desired and serving differ, or the drain observes a
newer marker, it remains `EpochPending` and follows the normal rendezvous; a
heartbeat never synthesizes an epoch. Only after this initialization does it
confirm the heartbeat and admit registrations. This uses the chosen failover
gap without persisting a second relay-owned observed-epoch ledger.
A `Resetting` Datastore instead resumes its exact `Quiesce` or `Restore` phase
under the exclusive fence and complete-head snapshot; it never takes this
retained-slot drain or topology-only seed path.

The sole substitute for an unreplayable pending marker is the complete-head reset
handshake. Slot loss, classification and identity rotation all follow 7.2's
reset-before-drop-before-create order under the exclusive Datastore fence. After
`Quiesced` proves the old slot absent, the authorized service submits every head's
`(database_id, physical_schema, epoch, last_rotation, checkpoint_sha256)` sorted
by id. Classification finalize carries it under 3.6; invalidation and identity
reset use the relay's column-limited read grant and POST closed body
`ResetHeads { relay_id, leader_term, heads: [DatastoreHead] }` to
`/internal/v1/cdc/datastores/{datastore_id}/resets/{reset_generation}/heads`
under a single-use relay assertion for the control audience.

`DatastoreHead` is that exact tuple. Control first checks the durable result keyed
by Datastore and generation: an exact-current-pair, byte-identical replay in
`Restore` or `Ready` returns its original revision; a changed replay conflicts.
A first application row-locks cluster and Datastore, requires the exact leader,
`Quiesce`, kind `SlotInvalidated` or `SystemIdentityChanged`, and an accepted
current-term `Quiesced` ACK. It then validates the exact topology set and 3.6's
desired/head rules, creates or adopts each actual pending epoch record, persists
the result and vector, and atomically enters `Restore` without changing kind.
Any stale term, wrong kind, phase or generation,
or partial or extra vector refuses without state change. `ResetHeads` cannot
complete classification, nor can classification finalize complete another kind.

Only that `Restore` snapshot authorizes fresh-slot creation, heartbeat and
observed-epoch seeding. The `Quiesced` drain plus durable reset state proves it
includes every earlier head commit; later ordinary writers retry after `Ready`, reacquire shared and put any
new marker in the fresh slot. Unqualified desired topology cannot seed observed,
and this exception never applies to ordinary reconnect or retained-slot failover.

**Zero tokio.** The design uses `compio-postgres`, `compio::time::sleep`
(`wal_consumer.rs:825`), `flume` (`change_stream_pg.rs:188-189`), compio `ntex`
and `cyper`; it adds no async runtime.

### 6.3 Resume, confirmation and same-term dedup

**PostgreSQL resume.** Each Datastore stream starts at its exact slot's
`confirmed_flush_lsn`. `start_lsn = "0/0"` asks PostgreSQL to use that position
(`libs/compio-postgres/src/replication.rs:826-830`). This is not a worker resume
coordinate; worker resume uses `AppCursor` from 5.2.

**The crash contract is state convergence with an explicit gap, not
at-least-once event delivery.** The ring is volatile. Once a complete
transaction's frames have been admitted to the appropriate app rings, the relay
may confirm its commit LSN without waiting for a worker acknowledgment. If the
process dies after that confirmation and before a cursor reads the frames, the
replacement cannot replay them from PostgreSQL and does not pretend otherwise:
the replacement first acquires a higher leader term, and every app registration
from an older term receives `Registered::Reset(RelayFailover)` before any
new-term app data.

The live-query path refetches current state and converges. Raw `db.subscribe`
consumers are told that an interval was lost and must refetch; the API is not a
lossless audit log. Duplicates may also occur around failover. This is the
accepted weaker contract.

The driver makes the discarded alternative impossible to hand-wave:
`flush_lsn` is a durability promise that permits PostgreSQL to recycle WAL, and
buffered bytes are not durable
(`libs/compio-postgres/src/replication.rs:988-1002`). The current API accordingly
requires acknowledgment or persistence before `advance_lsn`
(`libs/compio-postgres/src/replication.rs:1407-1418`). Volatile ring admission
satisfies neither precondition. The relay implementation therefore **replaces**
that method, with no alias, by an explicit contract:

```rust
enum ConfirmationBasis {
    DurableHandoff,
    GapFenced { leader_term: u64 },
}

ReplicationStream::confirm_lsn(lsn, basis)
```

`DurableHandoff` preserves the existing persistence-or-acknowledgment contract.
The relay uses `GapFenced` only after `leader_term` has committed in the
coordination database and the coverage rule below is true. It states openly
that PostgreSQL may recycle the WAL even though row frames are not durable,
because loss of the volatile state forces a higher-term reset. Any loss of ring
or sequence state makes the leader self-demote rather than continue in the same
term. The confirmation contract is weaker than durable delivery; the durable
fact is the monotonic term that makes the gap observable.

**Why this choice.** Coupling confirmation to every worker acknowledgment lets
one slow or dead worker pin a Datastore slot and grow cluster-shared WAL, which
violates 5.4. A persistent spool could provide at-least-once delivery, but it
adds a second durable row-data store, retention and deletion policy, encryption
and access-control surface, replication, and write amplification on every
change. This proposal accepts refetches and a weaker raw-event contract instead
of creating that system. If lossless raw CDC becomes a product requirement, a
durable spool is required; acknowledgment coupling remains rejected.

**Confirmation and the clamp.** `wal_consumer.rs:449-497` advances on `Commit`
and sends a standby status update; the long comment at `:459-494` records why a
mid-transaction `wal_end` is safe to report. The extracted relay decoder retains
that reasoning.

`wal_consumer.rs:441` does `stream.advance_lsn(wal_end)` on `PrimaryKeepalive`.
Under the driver's stated durability contract that call is not a valid promise,
and copying it would be data loss. Merely deleting it would create the other
failure: an idle database pins cluster WAL forever. `wal_end` is the SERVER's
current end of WAL
(`libs/compio-postgres/src/replication.rs:1064-1071`), which can be arbitrarily
far ahead of anything the relay has decoded and admitted. Confirming it would
skip changes inside the current term without putting either their frames or a
covering `AppReset` in the ring, and PostgreSQL would then be free to recycle
them.

**Specification: `min(wal_end, highest_gap_covered_lsn)`.**
`highest_gap_covered_lsn` advances only after every affected app is covered by
one of three states: every frame through that position is appended to its ring;
the decoder proved the interval contains no published frame for it; or an
`AppReset` was appended before the app entered an active reset state that covers
the dropped interval. It means "replayable in this term, explicitly reset in
this term, or reset on the next term", not durable delivery. Coverage advances
only to a decoded `Commit.end_lsn`. A keepalive is not decode evidence and never
advances it.

**Idle progress comes from a decoded heartbeat transaction.** Provisioning each
Datastore creates one reserved
`__zeroship_cdc.heartbeat(id boolean primary key check (id), nonce bigint not
null)` row and includes exactly `(id, nonce)` in that Datastore's shared
publication. The table is owned by an operator no-login role. The relay's
separate least-privilege SQL role receives `UPDATE (nonce)` on that table and no
creator-schema privilege; app and worker roles receive none. When
`wal_end > highest_gap_covered_lsn` and no decoded commit has advanced coverage
for 10 seconds, the relay executes
`UPDATE __zeroship_cdc.heartbeat SET nonce = nonce + 1 WHERE id = true` and
commits. It recognizes that reserved relation, never fans it out, and advances
coverage only when pgoutput delivers the heartbeat transaction's decoded
`Commit.end_lsn`. One transaction per idle Datastore per ten seconds, plus one
ordinary SQL connection per Datastore, is the accepted write and connection
cost. Granting `pg_logical_emit_message` to the relay is rejected because that
role could then forge section 8's authoritative epoch markers.

Unrelated WAL in another database may therefore remain behind this slot for up
to the heartbeat interval. That bounded retention is intentional. It replaces
the false claim that a keepalive proves an idle Datastore has decoded through
the cluster-wide `wal_end`.

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
So the extracted relay decoder replaces all three calls with one `confirm(pos)` helper
that clamps and then calls
`confirm_lsn(clamped, ConfirmationBasis::GapFenced { leader_term })`. At `:454`
the clamp is a no-op by construction, because the commit is confirmed only
after every affected app has ring admission or active-reset coverage. The test
asserts that ordering.

**Same-system duplicate suppression. The key is
`(commit_lsn, change_index)`.** The
per-change LSN cannot be the key, and both reasons are measured on PostgreSQL
18.4.

*Within* a transaction, the ordinary case looks fine. Three single-row `INSERT`s
in one transaction:

```
    lsn    | xid | frame
 0/178B0B0 | 755 | B
 0/178B0B0 | 755 | R  public.t (id, a, ssn)
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
(`crates/zeroship-schema/src/query.rs:4243-4396`,
`INSERT INTO {schema}.{table} ({cols}) VALUES {tuples} RETURNING *`), so no
creator write reaches the collapse **today**. `COPY`, `CREATE TABLE AS` and some
`INSERT ... SELECT` plans do. "Not reachable from one caller today" is not a
property a wire format should be built on.

**The key that survives both.** `commit_lsn` is strictly increasing in delivery
order, because transactions are delivered in commit order and every commit record
occupies a distinct WAL position - visible in the interleave transcript above,
where B commits at `0/17DCFB0` and is delivered first while A commits at
`0/17DCFE0` and is delivered second, in spite of A's change being earlier in the
WAL. Within one transaction `commit_lsn` is constant and the decoded DML ordinal
strictly increases by construction. An app may observe gaps where rows routed to
other Grants consumed ordinals, but its observed pair remains strictly monotone
in delivery order. The rule is:

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

### 6.4 Watermark and subscriber state have different reset boundaries

Two pieces of worker state must not be conflated:

- A higher `leader_term` always invalidates the per-term `AppCursor` and
  subscriber state. It forces `Registered::Reset(RelayFailover)` before
  new-term data and clears the numeric `(commit_lsn, change_index)` watermark.
  Every higher-term leader necessarily opened a new Datastore replication
  connection, and an unchanged `(systemid, timeline)` cannot distinguish an
  ordinary reconnect from a same-timeline storage rewind. Duplicate raw events
  are the accepted cost of the safe rule.
- A changed `systemid` or `timeline` invalidates that numeric watermark as well,
  because the same LSN may now name different WAL. Every affected ring receives
  `AppReset(SystemIdentityChanged)`; the worker clears the watermark, refetches,
  and starts a new numeric history.

The complete watermark rule is closed rather than inferred from the reason
name. `Initial` has no prior watermark. `RelayFailover`, `GrantRebound`,
`DatastoreReconnect` and `SystemIdentityChanged` clear it because either the
Database or the WAL identity may have changed. `RingOverrun`,
`DatabaseEpochChanged`, `AppDegraded`, `AppReactivated`, `SlotInvalidated` and
`ClassificationChanged` retain it on the same
`(systemid, timeline, database_id, grant_generation)`. Every reason still
refetches live state. A reason absent from these two sets is a wire-decode error,
not a guessed default.

Thus a leader change is not claimed to replay "only duplicates". It may have
lost confirmed ring frames, which is exactly what the mandatory reset covers,
and it may redeliver older rows because the watermark was cleared. The event
feed is neither lossless nor exactly once across terms.

Each ring binding retains
`latest_watermark_clear: Option<(seq, ResetReason)>` after the corresponding
`AppReset` frame is evicted. A cursor with `next_seq <= seq` receives that
stronger reset reason, never a watermark-retaining `RingOverrun`; a cursor
strictly after it follows the ordinary range rule. This metadata lives for the
binding lifetime rather than `ring_retention`. A higher term has no old ring
metadata and clears through `RelayFailover` directly. The accepted metadata
cost is one sequence and one byte-sized reason per active binding.

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

The unchanged pair on crash recovery is not accepted as a silent inference.
Every same-term ordinary Datastore reconnect outside an active reset generation
emits `AppReset(DatastoreReconnect)` for its routed apps and clears their numeric
watermarks before delivery. A higher term uses `RelayFailover`; a reset-owned
fresh connection uses that generation's immutable reset kind and emits no second
reconnect reset. The ordinary rule covers a same-pair storage rewind and turns a
network interruption into refetches and possible duplicates. That cost is
accepted because the relay has no trustworthy server-incarnation signal.

**The relay already fetches the answer and throws it away.**
`ReplicationConnection::identify_system` returns
`IdentifySystem { systemid, timeline, xlogpos, dbname }`
(`libs/compio-postgres/src/replication.rs:816-821`, `timeline: u32` at `:818`),
and `wal_consumer.rs:377` discards the value:
`if let Err(e) = conn.identify_system().await { ... }` - the success arm has no
binding.

Specification:

- On every leader acquisition and every Datastore replication connect, the relay
  reads `(systemid, timeline)` from `IDENTIFY_SYSTEM` and records them beside
  the Datastore topology revision. Matching identity follows the scoped rule
  above: only a same-term ordinary reconnect emits `DatastoreReconnect`.
- A `systemid` different from the Cluster's
  `postgres_system_identifier` is first treated as a misrouted tenant DSN. The
  relay fails that Datastore closed and does not update topology, drop a slot or
  serve rows. A legitimate restore uses one operator-only
  `RotateCdcClusterIdentity(cluster_id, expected_old_pair, new_pair)` operation.
  It first proves every configured Datastore replication DSN reports the new
  `(systemid, timeline)`, then in one control transaction compare-and-swaps both
  durable identity columns, increments
  the topology revision and places every Datastore in
  `Resetting { phase: Quiesce, kind: SystemIdentityChanged }` at a
  new generation. A higher-term relay
  takes each exclusive Datastore fence, appends
  `AppReset(SystemIdentityChanged)` for every app, proves the exact old slots
  absent and acknowledges `Quiesced`. It must submit 6.2's complete-head
  snapshots before control advances the same generations to `Restore`; only
  that phase authorizes fresh slots, heartbeat decode and `RestoreReady` before
  `Ready`. This operational step is the
  accepted recovery cost; automatically trusting a changed identifier could
  connect a relay to another tenant cluster.
- With the expected system id unchanged, a timeline equal to
  `postgres_timeline` is current. A larger timeline is a promotion or PITR: the
  leader proves every Datastore connection reports it, submits a fenced
  `AdvanceCdcClusterTimeline(expected, observed)` control operation, and
  self-demotes. Control compare-and-swaps the stored timeline, places every
  Datastore into the same `SystemIdentityChanged`
  `Quiesce -> Restore -> Ready` state machine and
  requires the higher-term fenced reset and complete-head handshake above. A
  lower timeline fails closed for operator recovery. Persisting
  the expected timeline is what lets a replacement process make this decision;
  process memory is not the authority. The extra reset is accepted because
  reused LSNs are silently dangerous.
- `Hello` carries `(systemid, timeline)`; the worker applies the two reset rules
  at the top of this subsection. The leader term also refuses an older leader's
  connection (6.2).
- A relay that wants this check without a replication connection has an SQL
  oracle: `SELECT timeline_id FROM pg_control_checkpoint()`. MEASURED on 18.4, it
  returns `1` on a fresh cluster. Useful for the watchdog surface; the
  replication connection's own `IDENTIFY_SYSTEM` is the authority, because it is
  the same connection the stream runs on.

The promotion measurement is PostgreSQL 17.11 rather than target evidence. The
18.4 failover gate in 10.3 must repeat it. What is already verified in code is
that the value is on the connection the relay opens and is currently dropped on
the floor.

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
nothing. The original probe that produced the following table did not record its
server version, so it is not target evidence. The slot and walsender defaults
were repeated on 18.4 with
`SELECT name, setting, boot_val, unit, context FROM pg_settings WHERE name =
ANY($1) ORDER BY name` over the five displayed names:

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

The invalidation sequence was measured on that unversioned probe and must be
repeated on 18.4:

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
full disk. The lost-slot path reads `wal_status` (already modelled:
`SlotHealth.wal_status` at `replication.rs:331-335`, documented values `reserved`
/ `extended` / `unreserved` / `lost`) and uses this one canonical order:

1. publish `Quiesce` with kind `SlotInvalidated`, acquire the
   exclusive Datastore fence, cancel and join the decoder;
2. append `AppReset(SlotInvalidated)` to every affected ring, activate reset
   coverage, clear egress and close any response that cannot take the control;
3. drop the exact lost slot, prove it absent and acknowledge `Quiesced`;
4. submit and durably validate 6.2's complete-head snapshot, then publish
   `Restore`;
5. create the fresh exact slot at the current WAL position, decode its heartbeat,
   seed observed from that snapshot, acknowledge `RestoreReady`, observe durable
   `Ready`, and release the fence before worker data resumes.

Reset precedes drop, and drop precedes create. In the same relay term, existing
subscriptions process the shared `SlotInvalidated` reset before any fresh-slot
change. If recovery also changes process and term, registration instead observes
the stronger `RelayFailover`; the protocol does not promise that an evicted old
ring's slot reason survives a term change.

Note that `restart_lsn` becomes NULL on invalidation, which makes
`watchdog_query`'s `lag_bytes` CASE (`replication.rs:370-372`) return NULL. The
`wal_status` column is the only surviving signal, which is why it exists and why
the relay must not key its health check on lag alone.

**The launch value is `max_slot_wal_keep_size = '8GB'`.** Provisioning sets it
cluster-wide and reloads configuration; relay readiness refuses `-1` or a live
value different from 8192 MiB. The cluster reserves at least 12 GiB of free WAL
filesystem headroom beyond normal peak usage before a Datastore becomes
CDC-ready. No production WAL-rate measurement exists, so 8 GiB is an explicit
provisional assumption, not a derived SLO. The measurement that may revise it
is peak cluster WAL bytes per second multiplied by the accepted relay repair
window, plus the 18.4 invalidation overshoot observed under checkpoint churn.
Until that gate exists, every environment uses 8 GiB rather than choosing a
local value. The accepted cost is up to roughly that much retained WAL before a
slot is sacrificed and every affected app refetches.

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

So the decision is explicit: **live queries absorb resets by refetching, while
raw `db.subscribe` remains a non-lossless API with a creator-visible reset
obligation.** This proposal does not make that event unignorable. The accepted
cost is that creator code can mishandle the raw feed; the API must not be sold as
an audit log.

### 7.4 The current fatal-error classifier does not know about invalidation

`is_fatal` (`wal_consumer.rs:743-760`) matches on lowercased substrings: `58p01`,
`does not exist` conjoined with `replication slot` or `publication`, and
`invalid slot name`. The invalidation error measured in 7.2 is SQLSTATE `55000`
with the message "can no longer get changes from replication slot". **None of the
three arms match it**, so today's supervisor would classify it as transient and
retry it forever at the 30-second cap (`wal_consumer.rs:712`, `MAX_BACKOFF`). The
relay's classifier must key on SQLSTATE, and `55000` from `START_REPLICATION`
must route to section 7.2's fenced lost-slot recovery path rather than to
backoff.

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
  (`wal_consumer.rs:605-607`, `rel.namespace != self.app_id` today; in the relay
  it becomes the Datastore/Database/Grant lookup from 6.2). A crash in per-app code must not be able to
  be a crash in decoding.
- **Every per-app step is total.** A per-app step returns `Result`, and its error
  becomes a `Gap` (5.6) or an `AppReset` for that app. It does not return `()` and
  panic instead. This is checkable rather than aspirational:
  `clippy::unwrap_used`, `clippy::expect_used` and `clippy::indexing_slicing` at
  deny level on the relay crate, which `./tests/clippy_gate.sh` already runs under
  `--all-features`. A compile-time rule is cheaper than a catch-and-continue arm
  and does not create a second, quieter control path.
- **A panic that still happens kills the process, deliberately. Do not
  `catch_unwind` per-app work.** compio is one runtime; catching a panic inside a
  task leaves whatever it was mutating in an unknown state, and the state here is
  a slot cursor plus confirmation and dedup state. A relay that dies releases the
  session advisory lock and a standby takes over (6.2). A relay that limps is a
  silent per-app data-loss machine that still holds the lock.
- **One app cannot starve another.** The ring writer is O(1) per frame and
  allocates nothing proportional to subscriber count; connection tasks read their
  own cursors (5.4). That is the structural difference from the Supabase shape,
  in which the publisher walked its subscriber list.
- **A poisoned app is quarantined, and quarantine never touches the slot.** An
  app whose ring reaches `app_fault_threshold` projection/encoding failures
  within the rolling `app_fault_window` first appends
  `AppReset(AppDegraded)`, atomically marks
  the binding reset-active and invalidates every connection cursor, then stops
  writing its row frames at the tenant filter. A connection that cannot process
  the reset control is closed; a later registration receives
  `Registered::Reset(AppDegraded)`. While reset-active, dropped changes satisfy
  the third `highest_gap_covered_lsn` arm, so the relay keeps decoding and
  confirming the slot. Reactivation appends `AppReset(AppReactivated)` before
  any row frame and forces another refetch. Reactivation is attempted once after
  `quarantine_duration`; a failed probe re-enters quarantine for another full
  duration rather than spinning. The probe is the next app frame, encoded into
  a temporary buffer: success appends `AppReset(AppReactivated)` and then the
  frame, while failure emits nothing and restarts the timer. Connection-local
  `Initial`, `RingOverrun`, slow-consumer close and `RelayFailover` resets never
  count toward this threshold, so one hostile worker cannot quarantine an app
  for healthy workers. **A tenant that cannot be served must
  never become a tenant that stops WAL confirmation for everyone** - which is
  5.4's invariant reached from the other side. The accepted cost is two
  app-wide refetches around a quarantine interval.

### 7.6 What the relay measures, and why the obvious signals are blind

Take the inversion first, because it makes the standard runbook wrong.

The relay confirms the slot once frames are admitted to its volatile rings or
an explicit app-wide reset covers their omission (6.3), not once a worker has
them. **So `confirmed_flush_lsn` advances at FULL SPEED while delivery
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

The signals that are not blind are relay-side, per app or per Datastore as the
fact requires:

| metric | why it is not blind |
| --- | --- |
| `cdc_delivery_outage_age_seconds{app_id}` | **The delivery health signal.** Maximum, over connected cursors that owe a frame or reset, of time since that cursor last advanced or processed its reset. The cursor state preserves the start time across ring eviction and clears it only on progress or connection close; zero means every connected cursor is caught up. It depends on no worker report |
| `cdc_ring_depth_bytes{app_id}` / `cdc_ring_depth_frames{app_id}` | The buffer fills exactly when delivery fails, and it is what 5.5's knobs bound |
| `cdc_resync_total{app_id,reason}` / `cdc_gap_total{app_id,reason}` | The loss counters, split by the closed reason enums |
| `cdc_connected_workers{app_id}` | "No subscribers" and "every subscriber wedged" both leave the ring shallow, because 5.4 evicts. Without this they alert identically |
| `cdc_published_columns{datastore_id,database_id,collection}` | The bracket's failure window (3.6): a published set that is a strict subset of the declared wire set, with no migration in flight, means a shrink whose widen never ran |
| `cdc_slot_wal_status{cluster_id,datastore_id}` / `cdc_slot_safe_wal_bytes{cluster_id,datastore_id}` | Slot loss and remaining WAL budget belong to one Datastore stream; an app label would hide shared capacity |
| `cdc_leader{cluster_id,relay_id,term}` / `cdc_topology_revision{cluster_id}` | The election pair, term-permit/membership fence and fan-out authority an operator checks before interpreting delivery metrics |

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

**A relay stamping `(database_id, database_epoch)` from a cached map merely moves
the epoch-carrier problem. On a retained slot, emitting the marker into WAL
dissolves it. A deliberately discarded slot instead uses 6.2's fenced durable-
head snapshot and never guesses from a cache.**

### 8.1 Why stamping from a cache only moves the problem

A relay that reads the current database epoch from the control plane and stamps it
onto outgoing frames is comparing two things that are not ordered with respect to
each other: the WAL position of the change it is decoding, and the wall-clock
moment it read the map. Decoding lags production, so a relay that refreshes its
map at time T can stamp epoch N+1 onto events produced under epoch N.
The failure is silent and its window is exactly the decode lag, which is the
quantity this whole service is trying to make small and variable.

It is also not authoritative: a topology refresh and a WAL position have no
common ordering. Stamping from a cache means claiming an epoch the decoded row
may not have been written under.

### 8.2 The mechanism that dissolves it

PostgreSQL has an in-band marker: `pg_logical_emit_message`. Transactional
messages are ordered in the WAL with the transaction that emitted them, and
pgoutput delivers them as an `M` frame. The driver already decodes them:
`PgOutputMessage::Message { xid, flags, lsn, prefix, content }` at
`libs/compio-postgres/src/replication.rs:2043-2050`, requested by
`StartReplicationOptions::messages` at `:860-863` ("Deliver
`pg_logical_emit_message` payloads as `PgOutputMessage::Message`. Off, the server
omits them and the decoder's `M` arm never runs.").

The PostgreSQL 18.4 transcript in 3.6 is already target evidence for
transactional ordering: it decodes the marker's transaction before the first
post-membership change. The negative "omit `messages` and receive no marker"
measurement survives only on 16.15 and 17.11. The 18.4 gate repeats both the
positive ordering and that negative with the target call:

```sql
BEGIN;
ALTER TABLE t ADD COLUMN nickname text;
SELECT pg_logical_emit_message(
  true,
  'zs.database_epoch',
  'dbs_example:7'::text,
  false
);
COMMIT;
INSERT INTO t (id, ssn, __zs_raw__ssn, nickname)
VALUES (1,'***-**-6789','123-45-6789','nick');
```

With `messages = true`, the decoded order must be
`Begin, Message(zs.database_epoch), Commit, Begin, Relation, Insert, Commit`.
The `Relation` contains `id`, `ssn` and `nickname`, never
`__zs_raw__ssn`. With `messages` omitted, the 18.4 gate must reproduce the
16/17 result that pgoutput omits the marker without an error. The relay sets the
option explicitly; there is no fallback if the negative changes on the target
release.

Three properties fall out of that transcript:

1. The marker is delivered **even though it belongs to no publication**. It needs
   no membership decision and cannot be forgotten by publication reconciliation.
2. It is ordered **before** the data that follows it, by the WAL, with no clock
   and no cache.
3. The publication list, not the table shape, decides whether the added column
   appears in the later `Relation`.

**Marker authority is an ACL, not the absence of raw SQL.** PostgreSQL 18.4 has
two four-argument overloads, both with default PUBLIC execute, while the WAL
`Message` carries no emitting role
(`libs/compio-postgres/src/replication.rs:2043-2050`). Datastore provisioning
therefore executes:

```sql
REVOKE EXECUTE ON FUNCTION
  pg_catalog.pg_logical_emit_message(boolean, text, text, boolean)
  FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION
  pg_catalog.pg_logical_emit_message(boolean, text, bytea, boolean)
  FROM PUBLIC;
```

There is no `zeroship_migrate_service` role today. The current
`migrate_server.provision_database_url` is explicitly the service's superuser
DSN (`deploy/compose/docker-compose.yml:473-501`), so the separate migration
process remains able to call the text overload after both `PUBLIC` revokes. The
worker login, app roles and relay login receive no grant, and the bytea overload
receives no non-superuser grant. For a transactional message PostgreSQL says
`flush` has no effect; the explicit fourth argument `false` is retained to pin
the PostgreSQL-18 identity rather than to claim a latency difference. A
PostgreSQL-16 signature branch is not added.

### 8.3 Where it is emitted

Inside the **widen** transaction of 3.6's bracket, within the locked
`apply_ir_documents` body specified in 3.7. Then "the publication reached
its new shape" and "the database epoch advanced" are one commit, and the relay
learns both from the same ordered stream. The marker payload is a typed wire
encoding of `(database_id, database_epoch)` rendered to canonical ASCII, not an
app id inferred from a schema name.

**Not the shrink transaction.** Shrink narrows the published set to
`old_wire INTERSECT new_wire`, a state no descriptor describes; a marker there
would announce an epoch the frames that follow do not yet match. Widen is
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

The complete-head reset handshake is the only exception. It is authorized only
after an app-wide reset has declared the WAL interval lost, under the exclusive
fence and durable reset generation. It does not provide a fallback for a missing
marker on a retained slot.

The first argument is always `true`. A non-transactional message is outside the
contract and receives no fallback path.

---

## 9. The six requirements, answered

The source design hands this service the first three requirements explicitly
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:1211-1216`). Review of
the service added the three operating requirements below; they are binding here
rather than retroactively attributed to the index.

| requirement | where |
| --- | --- |
| A wire projection that is a WHITELIST over declared fields, covering `changed_columns` | Section 3. Publication column lists computed from `storage.valueColumn` over declared fields unioned with the replica identity (3.1), folded server-side from the same policy-resolved ops as the DDL (3.7), one shared publication bracketing the DDL (3.6), plus the `ProjectedTuple` newtype for the SQLite arm (3.4) |
| A MASK-ONLY fixture that fails if the raw sibling name or plaintext enters pgoutput or relay ingress | 10.2 |
| The schema-change signal | Section 8: `pg_logical_emit_message` in the widen transaction |
| Leader election and resume, without tokio | Section 6: one `pg_try_advisory_lock` per `cluster_id` in the designated coordination database; worker term permits plus the predecessor-membership fence; one Datastore stream per slot; worker resume through `AppCursor`; mandatory failover reset; numeric watermark reset on leader failover, Grant rebind, Datastore reconnect or PostgreSQL identity change |
| `max_slot_wal_keep_size` must be set | 7.2, plus 7.4 - setting the GUC without fixing the fatal classifier converts a full disk into an infinite retry loop |
| Measure the added latency; do not estimate it | Section 11. No figure appears in this document |

---

## 10. Test strategy

### 10.1 What the existing test rules on

The current synthetic test is
`a_change_event_publishes_no_values_and_no_raw_column_name`
(`crates/zeroship-plugin-db/src/broker.rs:1874-1936`). Unlike the stale text
this section replaced, the test already models the shipped storage flip:
`ssn` holds the mask and `__zs_raw__ssn` holds ciphertext
(`:1874-1904`). Its serializer assertions are useful, but it hand-builds the
`ChangeEvent`. It cannot prove what the migration service published or what
pgoutput sent.

The comment immediately before that test at `broker.rs:1853-1871` is stale and
describes the pre-flip parent/sibling layout, contradicting the test it
introduces. It is not evidence for this design and needs a separate code-comment
cleanup; the assertions at `:1874-1936` are the current fact.

Keep the `message_to_json` and safe `ws_frame` unit arms and name their narrow
serializer claims. The obsolete value-carrying helper is already deleted. These
arms are not the publication acceptance test.

### 10.2 The mask-only fixture

The replacement is a PostgreSQL-18.4 security integration test spanning the
real producer boundary:

1. Apply a real migrate-server bundle declaring an ordinary `patients.note`
   control and a **mask-only** `patients.ssn`
   (`t.string().mask(...)` with no encryption). This must exercise the
   `ResolvedIrDocument -> WireProjection -> bracket` path from 3.7. The creator
   does not declare `id`: the confined ceiling forbids author primary keys and
   mandatorily injects `id` plus six other system fields
   (`policies/confined-system-shape.inject.toml:72-123`).
2. Insert a row through the real plugin-db write pipeline, with a distinctive
   plaintext sentinel and a distinct mask. Direct SQL is not the write-side
   acceptance path.
3. Through an admin-only assertion connection, prove the physical row stores
   the mask in `ssn` and the exact sentinel in `__zs_raw__ssn`. This positive
   control catches a test that never placed plaintext in the raw column.
4. Query `pg_publication_tables.attnames`. It must equal the complete typed,
   policy-resolved projection, including every visible injected system field,
   rather than a hand-written three-column list. Separately assert that `id`,
   `note` and `ssn` are present and that `__zs_raw__ssn` and the historical
   `ssn_masked` spelling are absent.
5. Capture decoded pgoutput immediately after the driver decoder and before any
   relay or broker projection. The test-only capture is memory-only and never
   logs row bytes. Assert that `Relation` names `ssn`, never the raw or
   historical name; the tuple carries the mask at that position; and neither
   the raw name nor the plaintext sentinel occurs in the captured bytes.
6. Consume the real relay response through the worker decoder. Assert one
   positive `Change` reaches the broker, its creator-visible columns include
   `ssn` and `note`, its `ssn` value is the mask, and neither raw name nor
   sentinel occurs in any wire frame. This is the no-drop control.
7. Mutate publication rendering to omit the column list. The catalog arm, raw
   pgoutput-name arm and plaintext-ingress arm must all fail. If only the catalog
   arm fails, the ingress hook is on the wrong side of the security boundary.

The same gate has a second fixture for the supported explicit
`.mask({ kind: "none", classification: "phi" })` shape. Insert a unique
plaintext sentinel through plugin-db and prove through the admin connection
that the declared physical field stores it and no raw sibling exists. The typed
projection and `pg_publication_tables.attnames` must exclude that field, and
neither its name nor sentinel may occur in captured pgoutput or a relay frame.
A builder-level negative arm tries to make that field part of replica identity
and must be refused. This is the acceptance arm for
`WireExposure::ExcludedProtected`; without it the ordinary mask-only fixture
does not exercise the storage shape whose protected value remains in the
declared column.

This test is intentionally slow: live PostgreSQL, migrate-server, plugin-db and
relay are all required. The cost is accepted because a unit fixture downstream
of pgoutput cannot prove that plaintext never entered the relay process.

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
- **Wire golden and rejection corpus.** Commit exact hexadecimal golden bytes
  for one `SubscribeRequest` and all eleven frame tags, covering every nested
  enum discriminant, both `Option` tags, Boolean values, a typed id, a
  `u32` `change_index`, a full-width LSN and a nonempty `CellValue`. Decode and
  re-encode byte-identically. Negative vectors cover zero and max-plus-one frame
  lengths, max-plus-one request and cell lengths, unknown frame and nested tags,
  Boolean `0x02`, invalid UTF-8, a noncanonical typed id, an out-of-domain
  persisted generation, a false vector count, truncation and trailing bytes.
  Decode a `Registered` whose registration generation differs from its request
  and require a fatal response error before any prime or cursor mutation.
  Assert malformed cursor bytes are request-level `400 MalformedRequest`.
  Send an unacceptable `Accept` and wrong `Content-Type` and require
  `406 NotAcceptable` and `415 UnsupportedMediaType` respectively. Exercise
  versions zero and two with the required `426` header, a direct nonleader
  `503` with no `Hello`, and table-drive every closed HTTP problem code:
  `AuthenticationFailed`, `DuplicateApp`,
  `NonMonotonicRegistrationGeneration`, `ConflictingShard`,
  `MalformedRequest`, `NotAcceptable`, `ClusterMismatch`,
  `RequestTooLarge`, `UnsupportedMediaType`, `UnsupportedWireVersion`,
  `NotLeader`, `AuthenticationStoreUnavailable`, `TermAuthorityUnavailable` and
  `ConnectionCapacityExceeded`. Assert its exact status, problem content type,
  absence of `Hello`, and terminal or jitter-retry classification.
  Pair a permit with another valid worker credential and require
  `AuthenticationFailed` before redemption or app lookup.
  Pin the permit commitment golden: mutating any non-permit request byte must
  fail redemption, while hashing the permit into its own preimage must not match
  the zero-length-field digest used by issuance and redemption.
  A same-term pre-`Registered` EOF must discard primes without replacing the
  old feed. After a validated N+1 `Hello`, the same EOF must leave the old term
  fenced and the app reconnecting. Post-`Registered` EOF must retry frozen
  cursors under a new registration generation; a decode violation must not
  hot-loop.
- **Epoch marker.** A migration that changes a collection must produce the WAL
  `Message`. Only after the desired/observed/serving rendezvous may the relay
  append one worker `Epoch` to every active grantee app ring for that Database,
  ordered before its next `Change`. On PostgreSQL 18.4 decode the same
  transaction twice: with
  `StartReplicationOptions::messages = true` require the ordered `Message`, and
  with the option omitted require all surrounding DML but no `Message` and no
  decoder error. The second arm is the target-version negative promised by the
  evidence ledger; merely inspecting the option value is not a substitute.
  Also assert
  both four-argument overloads are revoked from `PUBLIC`, the migration
  service's current superuser provision connection can execute the text
  overload, and app, worker and relay roles each get `42501`. This arm must name
  the overloads, so the PostgreSQL-16 signature cannot accidentally pass.
- **Epoch commit rendezvous.** Run both races: deliver the marker before the
  desired topology revision, then deliver topology before the marker. In each
  case assert the relay pauses E+1 frames until desired, observed and serving
  epochs agree. In the topology-first arm a worker still carrying serving E
  must receive `EpochPending`, not `StaleExpectedBinding`; comparing it with
  desired E+1 first must fail the test. The topology ACK reports the matched
  Database epoch, and the epoch-commit POST returns only after control promotes
  the serving epoch.
  Hold confirmation below the marker commit, kill before the ACK transaction
  commits, and require the replacement to replay the marker. Repeat with the
  kill after durable ACK: the new leader must initialize observed from equal
  desired/serving topology and become ready even when no marker replays.
  In a third arm, invalidate the slot while desired is E+1 and serving is E;
  after reset-before-drop, the complete-head handshake and fresh-slot heartbeat,
  the reset must adopt the exact pending commit record and promote E+1 rather
  than wait forever for the lost marker. Its blocked ordinary POST must return
  the reset revision without another topology revision; a changed tuple conflicts.
  Then exercise the missing cross-product: commit T4 at E+1, crash the migration
  process before its first control POST so desired and serving remain E, and
  invalidate the slot in the same relay term. The reset snapshot must contain
  the exact E+1 rotation and checkpoint hash, durably advance desired, seed
  observed at E+1 and promote serving. Restart the migration service and send
  that first exact POST: it must return the reset-associated revision without a
  new epoch, revision or barrier. Omitting the head read, shared commit record or
  seeding stale desired E must strand this arm and fail it.
  Drop the first successful POST response and prove reposting the identical
  `last_rotation` and checkpoint hash returns the original revision without an
  extra epoch. Hold the relay barrier incomplete and prove the next migration
  refuses before T1; comparing only control's desired rather than serving epoch
  must make this test fail.
- **Service-assertion replay is fleet-wide.** Race one assertion through two
  relay verifier instances backed by the same
  `service_authn.service_assertion_replay` table and prove exactly one atomic
  claim wins. Use an assertion once, fail over to a higher-term relay while it
  remains within its lifetime, and prove replay to the replacement is
  `401 AuthenticationFailed`. Make the replay store unavailable and prove the
  relay returns `503 AuthenticationStoreUnavailable` before decoding the
  request body. Substituting `InMemoryReplayStore` must fail both replica arms.
- **Datastore cardinality and fixed publication.** In one physical cluster,
  create Datastores A and B. Assert each exact slot decodes only its own database,
  draining A's slot from B is refused, and the same slot name cannot be created
  twice cluster-wide. Assert each stream starts with exactly its one
  Datastore-derived publication. Add a Database to A and prove its first change
  arrives without restarting A's stream; add Datastore C and prove a new stream
  is required. With stock settings, reserve ten slots and assert provisioning the
  eleventh Datastore fails before it becomes CDC-ready.
- **Slot invalidation.** Keep process, `relay_id`, term, rings and responses alive;
  pause one decoder and churn under a small `max_slot_wal_keep_size` until `lost`.
  Recovery must order `Quiesce`, exclusive, cancel/join, shared
  `AppReset(SlotInvalidated)`, egress clear, exact drop/absence, `Quiesced`, exact
  head validation, `Restore`, fresh create/heartbeat, `RestoreReady`, `Ready`, then
  fresh `Change`. Every subscription processes broker `Resync`; an earlier
  `DatastoreReconnect` neither replaces it nor causes a second reset on creation.
  Assert SQLSTATE `55000`. A process-restart control observes
  `Reset(RelayFailover)`, not slot reason; moving drop before reset fails.
- **Leader election and coordination scope.** Two relay instances for one
  `cluster_id` connect to the designated `zeroship` coordination database while
  owning two Datastore streams. Assert exactly one holds the allocated lock key,
  both Datastore streams belong to it, a clean process kill releases the lock,
  and the survivor's `Hello` term is strictly greater.
  In the half-open arm, blackhole the coordination TCP path and assert the
  bounded probe trips and cancels all streams and worker responses. Confirm,
  ring-write, topology-ACK and worker-send failpoints after the cancellation
  token trips must all refuse; the test makes no impossible claim about work
  completed during the bounded detection interval.
  In the overlap arm, hold N's last permit after its cluster read; terminate N's
  coordination backend while withholding EOF and require N+1 update to wait.
  Commit the permit, withhold its response and N+1 `Hello`, finish the update, and
  hold N frames before broker admission and in the subscriber queue. The trigger
  and poll must expose the exact N+1 pair/challenge; mutation fails, and pending
  state rejects N's topology ACK and N+1's transition ACK.
  The worker learns N+1 only by control poll and pauses N delivery under its shared
  guard. Both ACKs wait without timeout. Guard release cancels N responses, purges
  queues, advances the pair, publishes resync and stores the receipt. Exact replay
  succeeds, changed challenge fails, then topology ACK may succeed. Still without
  `Hello`, delayed permit redemption and broker admission reject.
  For a second guarded worker, only exact death, credential revocation and bound
  `SupervisorFence` unblock; stale proof fails, receipt replay succeeds, and the
  process cannot resume. With N+1 update locked first, permit returns N+1 or
  `FenceRequired`. Race final N-to-N+1 ACK with N+2 and topology ACK with retry:
  update-first cannot skip the unfinished term; completion/ACK-first commits first.
  N+1 death still requires worker/supervisor completion; empty membership advances.
  Mutation-remove the definer trigger, cluster-first locks, pending guard,
  redemption, local fence, receipt/death proof or empty-set arm. Race release with
  issue/redemption: release-first increments the epoch and delayed operations reject
  while inactive; issue/redemption-first may succeed, then release drains and clears.
  Stale epoch or changed body changes no state; exact release/fence replay does not
  increment twice. Racing term increment must freeze or reject the stale pair.
  Wrong credentials leave membership byte-identical. N+1 fences the exact old slot
  and N's probe cancels. `zeroship_cdc_coord` updates only the leader pair; writes
  to fence, revision, identity or member state get `42501`.
  A negative fixture taking the same
  numeric advisory key separately in the two Datastores proves why those
  connections cannot be the election authority: both locks succeed. Register
  apps on two clusters from one worker and assert it resolves two
  operator-provided endpoints, sends one or more deterministic shards per
  cluster, and each relay rejects the other's `cluster_id`. Give one cluster
  term 20 and the other term 5; advancing the first to 21 must not cancel any
  response or alter the term fence for the second. Provisioning must
  reject two cluster ids with one `system_identifier` and one cluster id whose
  two Datastore DSNs return different identifiers. Break one Datastore stream
  and assert leader readiness and the healthy stream remain green while only
  the broken Datastore's app gets `DatastoreUnavailable`.
  On dedicated PostgreSQL-18.4 fixtures, take a base backup, start it with
  `recovery.signal` and promote it; `IDENTIFY_SYSTEM` must preserve
  `systemid`, increase the timeline, and drive the authorized
  `AdvanceCdcClusterTimeline` plus higher-term reset path. Before that reset,
  commit one head at E+1 while suppressing its control POST so desired remains
  E. In isolated first-application fixtures, a stale term, wrong generation,
  non-`Quiesce` phase, wrong kind, missing current-term `Quiesced` ACK, partial
  or extra vector, and migration finalize must each fail while preserving its
  prior state. Accept the exact current-term `ResetHeads`, pause in
  `Restore`, and replay its byte-identical body under a fresh assertion; it must
  return the original revision. A changed replay must conflict without changing
  that persisted snapshot or revision. Reach `Ready`, replay the identical body
  again, and require the same result. Recovery must bring desired, observed and
  serving to E+1. Separate vectors must reject both a head at E+2 and a head at
  desired E+1 whose rotation or hash differs from the stored tuple while serving
  is E. As the relay management login, select exactly the five granted head
  columns, then require `42501` when reading `projection_checkpoint` or pending
  claims and when updating any head column. In the control arm,
  crash-stop PostgreSQL, restart the same data directory through ordinary crash
  recovery, and require `IDENTIFY_SYSTEM` to preserve both values. That arm
  must still emit `AppReset(DatastoreReconnect)` and clear the numeric
  watermark. These two arms are the target-version replacement for the bounded
  PostgreSQL-17.11 observation in 6.4.
- **Registration sharding.** Register 1,025 apps in one immutable worker
  snapshot and assert the encoder creates exactly two requests, with app counts
  1,024 and one, one shared nonzero `registration_generation`,
  `shard_index` values zero and one, and `shard_count = 2`. No app may appear
  in both shards, and neither shard may fail `RequestTooLarge`. Drop the second
  response before `Registered` and prove its retry uses a fresh assertion but
  byte-identical body and the same admission key, atomically cancelling the
  half-open response while the first shard remains live. Repeat after the relay
  emits `Registered` but a proxy drops that frame before the worker reads it;
  the identical-body retry must supersede and complete, not return
  `NonMonotonicRegistrationGeneration`. Hold shard one before
  `Registered`, complete shard zero, then write to an app in shard zero and
  prove delivery begins without buffering behind its sibling; an app in shard
  one must keep its old selected feed. Changing one byte under the admitted key
  must return `ConflictingShard`, cancel both candidate responses and mark any
  already switched app reconnecting. Retry the poisoned generation with its
  original body and require the same `ConflictingShard` without a replacement
  response. Repeat `DuplicateApp` once with two entries in one shard and once
  across sibling shards; both must poison the whole generation, and neither
  may emit two outcomes for one app. After `Registered`,
  advancing a cursor must mint a new generation and regenerate every shard.
  While an N response remains selected for an app rejected by N+1, retry N with
  its exact original body. It must return
  `NonMonotonicRegistrationGeneration` without cancelling either the surviving
  N response or any N+1 response. Revoke or rebind an app after its accepted
  response is cancelled, then deliver a delayed identical request body; it must
  revalidate into the current rejection and never resurrect cached primes,
  authorization or rows. Make one relay process self-demote, reacquire a higher
  term, and deliver an identical old body; the old admission cache must be gone
  and any accepted cursor must name the new `Hello` term with
  `Reset(RelayFailover)`. Changing the app set likewise mints a new generation.
  If make-before-break cannot acquire the
  bounded egress permit, assert the documented break-before-make fallback
  closes only enough old responses to admit progress and marks their selected
  apps reconnecting.
- **Topology acknowledgment and Grant fan-out.** Bind two Grants to one Database;
  one row reaches both with the same `(commit_lsn, change_index)`. Hold old-binding
  rows in relay egress, worker decode and subscriber delivery, then revoke/rebind.
  Relay cancellation and topology ACK cannot expose it while worker poll is held;
  after the exact Grant fence arrives, a held shared guard still blocks worker ACK.
  Guard release purges all old-generation queues, installs the minimum generation
  and stores the receipt; delayed frames reject, only the remaining Grant gets the
  next row, and new permits wait. A second active worker with no matching app also
  needs exact receipt or supervisor death proof. Deleting the Grant-fence row,
  local floor or receipt predicate must admit a post-exposure row.
  Race permit issue/redemption with transition: either operation first leaves an
  active member that is snapshotted; transition-first blocks both through exposure.
  Race release with transition: release-first joins tasks and escapes the snapshot,
  invalidating delayed redemption; transition-first returns `FenceRequired`, stays
  active, completes under the guard, then retries release. Race Grant creation with
  term completion: Grant-first joins the term batch; term-first removes the member
  before snapshot. Orphan rows or early completion fail. Old-term relay ACK plus
  new-term worker receipts still waits for the successor's current-pair ACK. Hold revision R1's
  barrier on failed Datastore A, then publish R2 for healthy Datastore B.
  A cumulative R2 ACK must complete B's barrier while A remains pending.
  Replaying that ACK is idempotent; a future revision, an old term and a status
  below its barrier revision are each rejected and cannot complete either
  barrier. Reject a status vector with a missing or duplicate Datastore, a
  foreign Database or Grant, an unsorted or duplicate child, an inconsistent
  reset generation, or nonempty children under `Unavailable`. A
  `Quiesced` may satisfy only a `Quiesce` snapshot at its exact generation;
  `RestoreReady` may satisfy only `Restore` at that generation, and accepting
  it must publish `Ready` atomically. Mutation: collapse `Quiesce` and `Restore`
  back into one `Resetting` state and require the fresh-slot deadlock test to
  fail. Also replace the
  per-Datastore greatest-revision rows with one
  cluster-wide pending latch, or let an ACK with the prior
  `(relay_id, leader_term)` complete a barrier, and require the test to fail.
- **Crash after confirmation, before delivery.** Pause a registered worker's
  ring cursor, commit a row with a distinctive sentinel, and stop the leader at
  a failpoint reached only after ring admission and after
  `pg_replication_slots.confirmed_flush_lsn >= commit_end_lsn` but before the
  egress cursor reads the frame. Send `SIGKILL`, let the standby acquire a
  strictly higher term, and reconnect the worker with its old `AppCursor`.
  Assert `Registered` reports `Reset(RelayFailover)`, the worker publishes the
  corresponding local broker `Resync` before accepting any new-term `Change`,
  and the accepted cursor is the captured new-term `ring_next_seq`. Arrange and
  assert that the old cursor's `next_seq` equals the new empty ring's
  `ring_next_seq`, so bounds alone would accept it. The
  live-query refetch must observe the sentinel row; the raw subscription must
  observe the reset. Duplicates are permitted and counted. In the mutation arm,
  delete both stale-session mismatch checks (`relay_id` and `leader_term`), not
  only one, and require this test to fail. This is the crash window the old
  "zero re-delivered events" test could not see.
- **Registration and relation priming.** Resume inside a retained interval whose
  original `Relation` frame is older than the cursor. Assert `RelationPrime`
  arrives before the terminal `Registered` and the first replayed `Change`
  decodes without cached state. Assert `next_seq == ring_next_seq` resumes as
  caught up, one below `tail_seq` resets, and one above `ring_next_seq` is
  rejected. After a caught-up resume, write a future change without changing its
  relation and prove the current-generation prime decodes it. Rebind an app from
  Database A epoch one to Database B epoch one and prove the changed
  `database_id` and `grant_generation` force `GrantRebound` rather than resuming
  the old cursor. Swap cursors between two apps sharing one Database and equal
  numeric generations, then between two worker ids, and require
  `CursorBindingMismatch`. On a fresh empty ring assert
  `tail_seq = ring_next_seq` and exact-bound resume. Register an app with no
  relations and assert `prime_relation_count = 0` completes an empty batch
  without a preceding `RelationPrime`. Send primes followed by
  `Registered::Reset` and prove the worker clears old state but installs the
  staged `prime_batch_id` before decoding. Two primes with the same
  `relation_generation` are a duplicate even if byte-identical. A count
  mismatch, duplicate prime,
  reused batch id, or nonzero count for an unknown batch must close the
  response as fatal without exposing a partial relation cache.
  Table-drive all seven `RegistrationRejectCode` values. For
  `EpochPending` and `DatastoreUnavailable` assert jittered retry while accepted
  peers remain selected; for `StaleExpectedBinding` assert one immediate
  topology refresh then that backoff; for `AppNotInTopology` and `GrantInactive`
  assert no retry before the active lease set changes; for `CursorAhead` and
  `CursorBindingMismatch` assert a surfaced local invariant and no spin. Every
  mixed `Registered` must retain request order and leave rejected apps on their
  old feed.
  Reconnect one Datastore in the same term, change a relation shape, and prove
  `relation_generation` is not reused.
  Send stale `expected_binding` and require `StaleExpectedBinding` before cursor
  evaluation; send current expected binding with an old cursor and require
  `GrantRebound`. Combine a higher term with a rebind to a Database whose LSN is
  numerically lower and require `GrantRebound`, not `RelayFailover`, plus a
  cleared watermark so the first B row is not suppressed. Evict a
  `DatastoreReconnect` reset from the ring and reconnect from a cursor before
  its sequence; `latest_watermark_clear` must preserve that stronger reset
  rather than downgrade it to watermark-retaining `RingOverrun`. Table-drive
  every `ResetReason` through both the shared `AppReset` path and the
  connection-local `Resync` substitution path and assert both apply the same
  closed watermark decision. Change the leased app set and prove each accepted
  app switches when its own shard completes, each rejected app keeps its old
  selection, late frames from a superseded response are dropped by the
  selection fence, and an old response closes only after no app selects it.
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
  generate WAL in another Datastore, and let the server send a
  `PrimaryKeepalive` whose `wal_end` is beyond this stream's gap-covered
  position. Assert the standby status update remains exactly at
  `highest_gap_covered_lsn`: keepalive position alone is not decode evidence.
  Then idle the tested Datastore until the reserved heartbeat update commits and
  assert coverage advances exactly to the decoded heartbeat
  `Commit.end_lsn`, never to the keepalive's later `wal_end`. Finally, on an
  ordinary affected `Commit` whose frames were admitted, assert the clamp is a
  no-op. If that arm ever goes red, the admission ordering in 6.3 has been
  broken somewhere else. Assert every standby status update used
  `ConfirmationBasis::GapFenced` and mutation-delete that explicit basis so the
  driver contract gate fails.
- **The publication bracket.** A migration that drops the ordinary published
  `body` field.
  Assert it succeeds (without the bracket it aborts with `2BP01`), that the
  published column set is the intersection between shrink and widen, and that the
  epoch marker arrives in the widen transaction. **Mutation: move the reconcile
  back to after the DDL and assert the migration fails**, which is the only way
  to prove the bracket is what fixed it.
  Add a non-prefix journal arm where migration A is skipped and later migration
  B applies; the committed projection must include only B's exact identity.
  Crash after DDL and before T4, retry with the same exact-checksum bundle, and
  prove checkpoint recovery widens correctly without applying either migration
  twice. Crash again after T4 advances the Datastore head to E+1 but before any
  control POST. With control still serving E, the retry must take the exact
  head-ahead recovery branch, repost the durable tuple, and perform no shrink or
  DDL before serving reaches E+1. Reapply the complete already-effective bundle
  and require an empty `PlannedDelta`; replaying its `createTable` into the
  folded checkpoint must make this test fail. Include unchanged and changed
  repeatable identities so the engine's effective-set semantics, not naive set
  subtraction, decide the plan. On an existing Database with v1 through vN
  effective, submit squash S over that exact prefix. Both projection deltas
  must carry `RecordSupersessionNoOp`, S's `up` must never run, and
  `folded_schema`, publication, roles and epoch must remain byte-identical while
  `net_applied`, `last_rotation` and the supersession edges advance atomically.
  The fresh-Database control must classify S as `ApplyOps` and fold its `up`
  exactly once. Then run an all-skip apply and prove the no-rotation repair restores
  the pre-shrink catalog exactly, clears `pending_rotation`, and emits neither
  an epoch increment nor a marker.
  On a live 18.4 decoder, hold the `DROP TABLE` plus `ADD TABLE` membership swap
  uncommitted while polling, commit it, and write before and after. Require both
  row transactions and no observable interval with the relation absent. This is
  the target-version replacement for section 13's bounded 17.11 atomicity
  measurement.
- **Two Databases, one Datastore publication.** Reconcile Database A while
  Database B's tables are members; assert B's `attnames` are byte-identical
  before and after. Mutation:
  restore `ALTER PUBLICATION ... SET TABLE` and assert B's tables disappear. This
  is the arm that would have caught the shared-object defect.
- **Classification reset barrier.** Start with a plaintext `ssn` change retained
  behind the slot, then apply a migration adding its mask. Capture decoded
  ingress across both decoder instances. Assert the old decoder is cancelled,
  its replication socket is closed, and its task is joined before any
  `AppReset(ClassificationChanged)` is appended, ring is cleared, topology ACK
  is sent, slot is dropped, or publication shrink begins. The relay must then
  append the reset for every Datastore app, clear egress, prove removal of the
  exact slot, and only then acknowledge the prepare barrier. A fresh slot opens
  only after the bracket and the finalize revision changes the exact generation
  from `Quiesce` to `Restore`; no raw name or plaintext
  sentinel may occur anywhere in the process-wide capture from either decoder.
  Both an unrelated app and the changed app must refetch. Mutation: leave the
  old decoder task live or keep the old slot, and require the ordering or
  plaintext-ingress arm to fail. A relay-authenticated `ResetHeads` request for
  this classification-owned generation must fail without changing phase.
  With Databases A and B sharing a Datastore, table-drive B's provisioning, T1,
  no-rotation repair, journal-only convergence and T4 through the shared-fenced
  head helper while A starts reset. Exclusive, slot drop and `Quiesced` wait for
  B's commit. Suppress B's T4 POST: A's exact snapshot includes B and Restore
  converges desired=observed=serving=head; B's later POST returns that revision
  without a new epoch, revision or barrier. Partial fence, A-only snapshot or a
  separate commit record fails.
  In `Quiesce`, A's token used for B or changed `migration_apply_id` refuses before
  a transaction. Kill after `Quiesced`: finalize waits for successor exclusive and
  current-term ACK. Kill after snapshot: durable `Resetting` rejects unrelated T4
  and exact-token writes in `Restore` until that generation completes.
  Let ordinary T4 take shared, then publish `Quiesce` before `Ready` revalidation:
  it releases and retries before T4/publication mutex, then reacquires after reset.
  Reverse lock order as deadlock mutation. A blackholed lookup must release shared
  at three seconds, or ambiguous unlock closes the session, so exclusive proceeds.
  An unavailable reset participant refuses prepare before DDL. Make the first DDL
  identity fail after prepare: finalize carries `migration_apply_id`, unchanged
  head tuple and no outcome label, restores the checkpoint, creates the fresh slot,
  heartbeats and reaches `Ready` without an epoch. With two identities, commit the
  first and fail the second: exact journal convergence advances once, widens only
  to the first projection, completes serving rendezvous, then returns the error;
  document-atomic treatment fails.
  Crash after Prepare/`Quiesced` before T1. The same attempt/fingerprint recovers
  token and apply id; changed content refuses. After failed-attempt `Ready`, that
  key replays its result while a new key starts a generation. Timeout/restart never
  reopens. Kill in `Restore`: only the fresh slot opens, `RestoreReady` reports and
  no row serves before `Ready`. Drop finalize success and retry identical token/body
  under a fresh assertion: it returns the original; changed body/token refuses.
  Splitting finalize into epoch commit plus reset release must deadlock.
  Repeat with one bundle that renames plaintext `legacy_ssn` to `ssn` and
  renames its table, then classifies `ssn`; the renderer must follow the same
  `ProjectionFieldKey` through both operations and trigger the reset before DDL.
  Comparing only qualified names must make this arm fail. In another bundle,
  drop plaintext `ssn` and create a protected `ssn` under the mapped table name;
  the historical-name collision must trigger the same reset even though the
  new field has a different key. Mutation-delete either lineage rule and
  require its corresponding arm to fail.
- **Slow consumer cannot reach the slot.** One worker connection that stops
  reading its body; a second app writing normally. Assert the second app's
  delivery is unaffected, that `confirmed_flush_lsn` keeps advancing, that the
  stalled app's `cdc_delivery_outage_age_seconds` grows, and that the stalled
  connection is closed after `slow_consumer_timeout`. The floor for this arm is
  the number of frames the healthy app delivered during the stall: an arm that
  delivers zero frames passes every other assertion trivially. In a separate
  admission arm, consume every `egress_bytes_total` permit, assert the next
  subscription gets `503 ConnectionCapacityExceeded` before allocating a
  queue, close one response, and prove exactly one permit becomes available.
  Mutation: allocate before acquiring the semaphore and require the RSS-accounting
  assertion to fail.
- **Quarantine covers confirmation.** Produce five app-data
  projection/encoding failures inside `app_fault_window` and assert only the
  fifth, `app_fault_threshold`, enters quarantine. Independently cause ring
  overruns, stale cursors and slow-response closures and prove none increments
  that counter. On entry, assert `AppReset(AppDegraded)` is appended before the
  first intentionally dropped row, every cursor is reset or closed,
  `highest_gap_covered_lsn` advances through later dropped rows, and a healthy
  app keeps receiving. After `quarantine_duration`, make the next frame encode
  successfully into the temporary probe buffer; assert
  `AppReset(AppReactivated)` forces a second refetch before the probed change is
  admitted. A failed probe restarts quarantine instead. Mutation: drop rows
  without the active-reset coverage arm and require the confirmation invariant
  to fail.
- **Gap and Truncate.** An over-budget value emits `Gap` only for that row;
  `TRUNCATE` emits `Truncate`, and unchanged TOAST is `Unavailable`, not `Null`.
  Decode an ordinary UPDATE without old tuple and an identity-changing UPDATE with
  one: the latter carries only old identity as `pk`, derives new identity from
  `values`, discards other old fields and updates 5.3's delivered-pk set. Neither
  wire nor `ChangeEvent` has an old tuple. Repeat through SQLite preupdate.
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
- `t2` = relay writes the frame into the app's ring. This is the ordinary point
  where it may confirm under 6.3's gap-signaled contract; an `AppReset` coverage
  transition is the corresponding timestamp for an intentionally omitted frame.
  It is the volatile retention boundary, not durable delivery.
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

### 12.1 The relay is a single point of failure and failover loses an event interval

Today a worker's CDC failure affects that worker's apps. Under this design one
process being wedged stops every live query on the cluster, and the leader lock
means a second instance is a standby, not a second consumer. The failover time is
bounded below by how fast PostgreSQL notices the dead session and releases the
advisory lock, which is a TCP-keepalive-shaped quantity, **not measured**, and
not obviously fast.

Every failover also forces a live-query refetch and tells raw subscribers that an
incremental interval may have been lost. This is the direct cost of choosing the
volatile ring instead of a persistent spool. The central coordination database
is another availability dependency: uncertainty closes healthy Datastore
streams deliberately.

Worker fencing adds polling, permit redemption, durable membership, credentials
and trusted death records. One unreachable worker can block a term or Grant
transition indefinitely, and every revoke/rebind fences every active worker.
This availability loss is the strongest objection to the choice.

A bounded userspace lease is rejected: after its last clock check, a descheduled
process can resume inside delivery. Kernel expiry is enforceable but turns
scheduler delay into process death and adds another safety-critical mechanism.
Explicit ACK or proved death costs more coordination, but its delivery
precondition remains true; neither relay may manufacture the death proof.

### 12.2 It is the new bottleneck, and it is a single decode plus a single fan-out

One decode per Datastore is the PostgreSQL floor measured in 3.6 and 6.1. But the
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

`change_index` is a decoder-assigned pgoutput DML ordinal, not a server field.
Assigning it before filtering and fan-out removes topology as an input, but
correctness still rests on pgoutput replaying a transaction's DML in the same
order every time. Section 13 records a bounded PostgreSQL-17.11 measurement, not
target evidence: it does not cover 18.4, spill, concurrency or another session.
If that premise fails, the failure
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
doing a catalog read and a `DROP TABLE` + `ADD TABLE` per member table while the
Database lock remains held. Shared-Datastore publication transactions serialize,
and a destructive reset also blocks unrelated head commits until `Ready`; other
Datastores remain independent. The bracket introduces an operator-visible
shrunk-but-not-widened state. Those are the costs of avoiding a migration that
aborts whenever a creator drops an ordinary published column.

### 12.8 The relay's failure policy is ten interacting knobs and a lint

5.4, 5.5 and 7.5 introduce the ten configuration values enumerated in 5.5,
plus a deny-level lint set. "No knobs" deploys correctly by construction and a
ten-knob surface has a wrong setting available for every one. An operator who
sets `ring_bytes_per_app` too low converts
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
  durable-spool alternative rejected in 6.3 and costed in 12.1 becomes
  available at low cost. That is three
  changes to one crate, all of which its own comments already contemplate.
- **If Datastore count approaches the stock ten-slot ceiling**, the control
  plane must place new Datastores on another physical cluster or schedule a
  restart with higher `max_replication_slots` and `max_wal_senders`. Combining
  their decodes into one slot is not an option; PostgreSQL binds a logical slot
  to one Datastore.
- **If live queries turn out to be a niche feature** rather than a headline one,
  a per-worker slot with `max_replication_slots` raised (option D) is a smaller
  system, and the credential problem could be solved by giving the *worker* a
  second, `REPLICATION`-only login role used by a dedicated thread. That is not a
  boundary, per the AGENTS.md privilege invariant, and this design argues against
  it; but it is the cheap option and someone will propose it.

---

## 13. Evidence boundaries and remaining verification

This section separates bounded observations from target evidence rather than
letting either read as a portable fact.

- **The worker drops `BYPASSRLS`, but the old corpus claim was false.**
  `db/migrations-ts/20260702000800_policies_rls.ts:6-23` enables and forces RLS
  on nine platform tables and installs their policies. The gateway module
  documents four RLS tables, three of which it accesses as the non-bypass
  `zeroship_gateway` role (`crates/zeroship-gateway/src/rls.rs:1-20`); that
  does not make it the only
  RLS consumer. What matters here is narrower: these are platform tables, not
  creator Datastore relations, and no creator-schema policy justifies giving
  the process that runs creator code a bypass.

  The worker's own posture check refuses boot unless its role holds
  `REPLICATION` and `BYPASSRLS` and names logical decoding as their purpose
  (`crates/zeroship-worker/src/db_posture.rs:101-128`). Section 4 moves decoding
  out of that process. The same change therefore revokes both attributes and
  inverts the posture check to require their absence. The worker's full live
  suite runs under `NOREPLICATION NOBYPASSRLS`. If that exposes an undeclared
  platform-table dependency, the fix is a least-privilege grant and the
  applicable RLS policy, never restoring the bypass. The accepted cost is
  making those dependencies explicit.
- **The advisory-lock release latency after a hard leader kill.** 12.1. Still
  unmeasured, but the experiment is now specified, because an attempt showed the
  obvious version measures the wrong thing. **Three kill shapes are not
  equivalent:**
  1. *Killing the client process* (including `SIGKILL`) closes its socket, so
     the kernel sends `FIN`, the backend sees EOF and the session-scoped lock
     releases in about a round trip. This is the fast path and it tells you
     nothing about the case of interest.
  2. *`SIGKILL` on the backend* is not a leader kill at all - the postmaster
     treats it as a crash and restarts every backend in the cluster.
  3. *Node or network loss*, where no `FIN` is ever sent, is the real case. The
     server keeps the session until TCP gives up, which is what makes the answer
     keepalive-shaped.
  So the experiment must partition the network (`docker network disconnect`, or
  an `iptables ... -j DROP`), never kill a process, and it must run against a
  **dedicated** cluster: shape 2 restarts every backend, which would void any
  other suite sharing the server. The answer is then governed by
  `tcp_keepalives_idle`, `tcp_keepalives_interval` and `tcp_keepalives_count` on
  the server side, and those settings - not the kill - are what the measurement
  should vary.
- **The pk-membership replacement for `old_tuple`.** 5.3. Specified, not
  prototyped, and it changes a subscriber-side contract.
- **Behaviour of non-transactional `pg_logical_emit_message`.** 8.4. Not used and
  not tested.

- **Section 8's `messages 'true'` requirement reproduces on 17.11.** Not a new
  finding - 8 already measures it on 16.15 and quotes the driver's own note
  ("Off, the server omits them and the decoder's `M` arm never runs"). Repeated
  on `server_version_num=170011` with a transaction containing an insert, a
  transactional `pg_logical_emit_message`, and a second insert: without the
  option the stream decodes `Begin Relation Insert Insert Commit` and the payload
  is absent; with it, `Begin Relation Insert Message Insert Commit` and the
  payload is present. So the behaviour is stable across two majors, and the
  ordering claim holds on both - the `M` frame lands between the two inserts,
  where it was emitted.
  Worth stating once in implementation terms, because the failure is silent:
  **without the option the stream is well-formed and complete-looking**, so a
  relay that omits it decodes data changes correctly forever while every
  schema-change signal vanishes. That belongs in a test that fails when the
  option is absent, not only in a doc comment.
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
- **LSN reuse across a divergence point.** The timeline change itself is
  measured - a basebackup restored with `recovery.signal` and promoted moves the
  timeline `1 -> 2` on an unchanged `system_identifier`, and crash recovery moves
  neither (6.4). What remains unobserved is a *consumer* resuming across that
  divergence and encountering reused LSNs; the gate is specified and its input
  is proved to move, but the failure it prevents has not been reproduced.
  Section 6.4 no longer depends only on that signal: every same-term ordinary
  reconnect outside a reset generation resets state even when the pair matches.
- **`heap_multi_insert` IS reachable, through creator migrations. Enumerated.**
  The data plane is clear: `env.db.insertMany` emits multi-`VALUES`
  (`query.rs:4243-4396`), and a three-row multi-`VALUES` insert was measured
  producing **three separate `Insert` messages**, not a collapsed one. But the
  migration guard permits two paths that do collapse:
  - **`COPY ... FROM STDIN`** is explicitly allowed. `check_statement_kind`
    denies only `COPY ... PROGRAM` (`rule::COPY_PROGRAM`) and a `COPY` naming a
    file (`rule::COPY_FILE`); the remaining arm returns `Ok(())` with the comment
    "Plain COPY ... TO STDOUT / FROM STDIN - safe"
    (`crates/zeroship-migrate-postgres/src/guard/sql.rs:1396-1408`). Safe against
    RCE and filesystem access, which is what that rule is for - and orthogonal to
    decoding shape.
  - **`CREATE TABLE AS`** is gated by target ownership, not denied: the
    `CreateTableAsStmt` arm resolves the relation and calls `gate_raw_create`
    (`guard/sql.rs:922-932`), so a creator may CTAS into their own schema.

  **So `change_index` is load-bearing rather than defensive.** 6.3 rejects the
  per-change LSN because it collapses under `heap_multi_insert`; that collapse is
  reachable by a creator today, without any new feature. 12.6's
  ordering-free alternative is therefore a genuine fallback, not a
  belt-and-braces option.
- **The swap IS atomic to a decoder running through it. Measured, and it
  exposed a limit the argument did not predict.** On PostgreSQL 17.11: a table
  published with `(id, pub, secret)`, a row inserted, then
  `BEGIN; ALTER PUBLICATION p DROP TABLE t; ALTER PUBLICATION p ADD TABLE t (id,
  pub); COMMIT;`, then a second row. Decoding the whole range from a slot created
  before the swap gives `Begin Relation Insert Commit` twice - **two
  transactions, not three.** The DDL transaction emits no change messages at all,
  and each data transaction carries its own `Relation` message reflecting the
  column list in force at that WAL position. The catalog afterwards reads
  `id,pub`.

  **The limit: the projection is prospective, not retroactive.** In the same
  decode, the pre-swap row still carries `secret` and the post-swap row does not.
  So "PostgreSQL never puts the excluded bytes on the wire" holds for changes
  decoded *after* the shrink, and **not** for changes already in the WAL when it
  ran. A relay resuming from a watermark older than a shrink will emit the old,
  wider column set for everything between that watermark and the swap.

  That is not a defect in the bracketing - 3.6's shrink-before/widen-after exists
  to survive `2BP01`, and it does. It does settle the security decision:
  ordinary projection edits are prospective, while an existing field becoming
  masked takes 3.6's mandatory Datastore slot-reset barrier. The barrier discards
  old WAL and forces every app to refetch. There is no conditional
  "retroactive-redaction mode" and no path that replays across the classification
  boundary.
- **Everything in 5.4, 5.5, 7.5 and 7.6 is specification, not observation.** No
  ring exists to overrun, no consumer exists to stall, no metric exists to read.
  Those four subsections state policy the implementation must satisfy, and the
  tests in 10.3 are how it gets checked.
- **The Vitess, Supabase, Salesforce, Kinesis, DynamoDB, Neon, Debezium and
  Materialize claims are relayed from the prior-art study and unverified.** They
  are cited as prior art shaping a decision, never as evidence about this code.
