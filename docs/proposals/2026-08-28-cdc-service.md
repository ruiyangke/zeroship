# The CDC relay service

**Status.** PROPOSED. **The relay is NOT BUILT, and no part of this document has
been implemented.** Four pieces AROUND it have landed, each of which makes the
relay cheaper to write and none of which is the relay:

| landed | what it is | commit |
| --- | --- | --- |
| `crates/zeroship-cdc-wire` | The leaf wire contract: framing, the eleven frames, `SubscribeRequest` and its permit commitment, the closed enums, and the identity and generation vocabulary. No I/O; `sha2`, `serde`, `serde_json` only, with `zeroship-core` a DEV dependency so the typed-id oracle runs without putting an HTTP client in a no-I/O crate's closure. 35 tests. | `05d9462c8` |
| `crates/zeroship-cdc-transport-spike` | The ntex-plus-cyper streaming pair, proven as 12 tests. Answers most of Open 4. | `f7e043252` |
| `crates/zeroship-data-cdc-server` | **The CRATE, not the SERVICE.** A manifest, a config module and a `main` that answers `--check-config` and then refuses to start. **ZERO files were moved into it**, which is the settled answer rather than a deferral. | `545ceff1e` |
| SQLite commit-window suppression | The suppression sample point moved from drain to commit. Ships behaviour; INERT today. | `8672dd355` |

**THE CRATE'S NAME IS `zeroship-data-cdc-server`, AND THIS DOCUMENT SAID
`zeroship-cdc` IN FIVE PLACES UNTIL 2026-09-03.** Every one is corrected below.
The old name was a working name, never a decision; the current one is an operator
ruling and is what `crates/zeroship-data-cdc-server/Cargo.toml` declares.

**It is a BINARY that nothing links.** Not a library the worker, the adapter or
anything else depends on. That shape is not packaging preference - it is what
makes the privilege boundary real, because a boundary any crate can link is the
appearance of one. The enforceable form of the rule, and the reason it is not
literally "no crate may depend on it", is in
`crates/zeroship-data-cdc-server/Cargo.toml`; the mechanical check is
`tests/data_crate_closure_gate.sh` arm 3.

**Contracts do NOT live in the relay and are NOT re-exported from it.** They have
two homes and the split is deliberate: the WIRE contract - anything the worker
and the relay exchange - is `zeroship-cdc-wire`; DATA-PLANE contracts are
`zeroship-data-core`. See "Where the contracts live" below for why routing the
wire types into `data-core` would break both crates' stated properties at once.

The consumption path the relay replaces still lives in
`crates/zeroship-plugin-db/` (`wal_consumer.rs`, `replication.rs`,
`slot_reaper.rs`, `change_stream_pg.rs`) - `git diff --name-only 8672dd355..HEAD
-- crates/zeroship-plugin-db` returns zero files - and the pieces that stay are
`crates/zeroship-data-core/src/broker.rs`, `read_set.rs` and
`cdc_lifecycle.rs`. It is still **blocked twice**, and creating the crate
unblocked neither: on the Datastore/Database/Grant entities of
`docs/proposals/2026-08-28-app-database-decoupling.md`, none of which exist, and
on a platform move to PostgreSQL 18.4 that the deployment does not make
(`deploy/compose/docker-compose.yml:73` pins `postgres:16`).

**`DatastoreId` remains a wire type with no entity behind it.** Measured:
`grep -rl DatastoreId crates/ db/` returns six files, five in
`crates/zeroship-cdc-wire` and one rustdoc mention in
`crates/zeroship-data-cdc-server/src/lib.rs`. No table, column or control-plane
record mints one. `cdc_clusters` and `__zeroship_admin.database_heads` still have
zero occurrences in `crates/` or `db/migrations-ts/`.

**The transport foundation is VALIDATED as of 2026-09-03, and the sentence that
stood here was wrong on its facts.** It read: "`ntex` v3 response streaming
(`Cargo.toml:46`) and `cyper`'s `stream` feature (`Cargo.toml:49`) are both
present in the workspace and no code has been written against them." Both
citations are right and the claim after them is false, and was false when
written. ntex response streaming ships in the gateway -
`crates/zeroship-gateway/src/proxy.rs:419` and `:876` return
`ResponseBuilder::streaming`, and `crates/zeroship-gateway/src/router/static_serve.rs:344`
and `:375` return `SizedStream`. cyper's `stream` feature ships in the runtime's
`fetch` and in the S3 client -
`crates/zeroship-runtime/src/web/fetch/http_network.rs:144`,
`libs/compio-s3/src/client.rs:460`, `:532` and `:1223` all call
`Response::bytes_stream`. What had no evidence was the PAIR, and the two
properties the relay actually rests on: that the worker sees frame N before the
relay has produced frame N+1, and that a worker which stops reading cannot stall
the ring writer.

That premise error is worth keeping rather than deleting, because it is the shape
this document is most exposed to: a true citation followed by a false claim about
what the cited thing is used for. The gate rules on the path and the line, never
on the sentence.

`crates/zeroship-cdc-transport-spike/` is that evidence, as twelve tests anyone
can re-run with `cargo test -p zeroship-cdc-transport-spike`. It is a spike, not
a relay: no election, no slot, no pgoutput, no frames from `zeroship-cdc-wire`.
Measured on ntex 3.7.2 and cyper 0.8.3 (the versions in `Cargo.lock`):

- **Incremental in both directions.** The handler will not produce frame N+1
  until the client acknowledges frame N, so a buffered response deadlocks rather
  than passing; the shared transcript came back
  `produced 0, consumed 0, ... produced 7, consumed 7`, strict alternation, and
  8 frames arrived as 8 separate 11-byte transport chunks. The response carries
  no `Content-Length`.
- **On compio, with no tokio reactor.** The ntex handler and the cyper client
  each assert `compio::runtime::Runtime::try_with_current(..).is_ok()` at
  runtime, and the process's own thread names after the exchange are the libtest
  threads plus `futures-timer` - no tokio worker. The linked-not-driven edge
  AGENTS.md describes is unchanged in kind but did move in size: the spike's dev
  dependency on cyper took `PINNED_ENTRYPOINTS` in `tests/zero_tokio_gate.sh`
  from nine names to ten, and the gate went red until the pin and the AGENTS.md
  sentence were updated with it.
- **ntex applies write backpressure to the response body**, which is the
  mechanism the ring design needs and had assumed.
  `ntex-3.7.2/src/http/h1/dispatcher.rs`, `poll_send_payload`, runs
  `ready!(self.io.poll_flush(cx, false))` before every `poll_next_chunk`, and
  `poll_flush(_, false)` in `ntex-io-3.9.2/src/io.rs` returns `Pending` with
  `BUF_W_BACKPRESSURE` once the write buffer reaches half its maximum. Measured
  against a client that stopped reading: the socket path absorbed 666 frames /
  2,730,600 bytes and the body stream then stopped being polled at all. That
  byte figure is this machine's, not a constant - `net.ipv4.tcp_wmem` maxes at
  4,194,304 and `tcp_rmem` at 6,291,456 here.
- **"The relay sheds, it never blocks" is expressible as written.** An egress
  buffer whose `push` is not `async` and takes `&self` compiles as an ntex
  response body. With a 64 KiB buffer and a stalled consumer it shed 215 frames
  while the producer kept looping (iteration 1 to 112 across the measurement
  window), and a 500 ms `slow_consumer_timeout` closed the response, after which
  the client drained the socket and saw EOF. The one-variable control - same
  producer, same rate, same buffer, a consumer that reads promptly - shed 0 and
  delivered all 4,096 frames / 16,793,600 bytes byte for byte.

**What the spike does NOT establish, so nobody reads it as more than it is.** It
runs plaintext HTTP/1.1 on loopback. TLS terminating in the relay process is
untested and needs an ntex feature the workspace does not enable today; so are
HTTP/2, the `application/problem+json` error surface, `503
ConnectionCapacityExceeded` admission, the global egress byte semaphore, and
every cost question at real fan-out. A multi-minute hold and a many-subscriber
fan-out are both still unmeasured.

**What the crate contains, so "the crate exists" is not read as "the relay
exists".** `crates/zeroship-data-cdc-server/` is three source files -
`src/lib.rs` (the crate rustdoc plus one exported refusal string), `src/config.rs`
(`CdcServerSettings` and its tests), `src/main.rs` (bootstrap, `--check-config`,
then refuse). There is no decode loop, no slot, no election, no listener and no
frame. `main` fails closed rather than idling, which is the shape
`zeroship-workflow-scheduler` already carries: a `platform` binary registered
ahead of its loop, because the `platform` classification is a REQUIREMENT to
register a configuration surface rather than a judgement call
(`crates/zeroship-config-contract/src/registry.rs`).

**SQLite CDC suppression now samples at COMMIT, and it is INERT.** `8672dd355`
moved the suppression sample point from drain time into SQLite's `commit_hook`,
so the window a guard covers is the set of commits made inside its scope and the
publisher never re-samples: the answer rides the packet as a per-event stamp
(`crates/zeroship-data-sqlite/src/change_sink.rs`,
`crates/zeroship-data-sqlite/src/cdc.rs`). The decisive argument was not that
dequeue-time sampling was racy but that it had **no defined answer for strictly
sequential code** - the channel is `flume::unbounded`, so a guard engaged,
written under, and dropped, with no concurrency anywhere, still produced a result
that was a function of publisher scheduling. That matters here because it is the
SQLite half of the same suppression handshake the relay's Postgres half will need.

It ships behaviour and exercises none of it in production. `BrokerPauseGuard::new`
and `SchemaPendingGuard::new` have no production callers: every non-test
occurrence in `crates/` is a comment or rustdoc, and the only real call sites are
in `crates/zeroship-plugin-db/tests/sqlite_integration.rs`. Read it as a
correctness fix to a mechanism waiting for its caller, not as a shipped feature.

---

## What it is

One binary, one process, executing no creator code. **The crate is
`zeroship-data-cdc-server`; this section said "Working name `zeroship-cdc`" until
2026-09-03.** It consumes PostgreSQL logical replication on behalf of the whole
fleet and pushes projected row changes to workers, so the worker process stops
speaking the streaming replication protocol and stops needing `REPLICATION` or
`BYPASSRLS`.

**Nothing links it.** The relay is a binary, not a library with a binary
attached. Read the negative form carefully, because the literal wording collides
with a live test and the collision is instructive: a workspace bin target must
carry a `[[package.metadata.zeroship-config.targets]]` class
(`crates/zeroship-config-contract/src/metadata.rs`), and a `platform` binary must
then appear in `DECLARING_BINARIES`
(`crates/zeroship-config-contract/src/registry.rs`), which
`crates/zeroship-config-contract/tests/real_registry.rs` compares for exact
equality against the class cargo metadata reports. Classifying the relay anything
else to dodge that would leave a shipped service's configuration surface
unaudited, which is the vacuity that test exists to prevent. So the property that
is both true and enforceable is:

> **No SHIPPED binary's normal-dependency closure may contain
> `zeroship-data-cdc-server`.** `zeroship-config-contract` - itself
> `class = "test-dev-tool"`, never shipped - is the single named exception.

`tests/data_crate_closure_gate.sh` arm 3 enforces it by inverting `cargo tree -i`
over the bin-package set derived from `cargo metadata`, rather than from a list in
the file. It rules on the exception too rather than skipping it, because a
mistyped package name, a bad flag and a genuinely absent edge all produce the
same empty output - so the arm proves its own instrument on every run.

**`cargo tree -e normal` cannot see dev-dependencies**, and that is the invisible
way this rule gets breached. `zeroship-migrate-server`, the declared peer, already
carries five of them. Nothing here catches a dev-dependency on the relay.

### Where the contracts live

Anything two processes must agree on is a CONTRACT, and no contract lives in the
relay or is re-exported from it. There are two homes:

- **`zeroship-cdc-wire`** owns the WIRE contract - framing, the eleven frames,
  `SubscribeRequest` and its permit commitment, and the `DatastoreId` /
  `ClusterId` / `DatabaseEpoch` / `GrantGeneration` vocabulary.
- **`zeroship-data-core`** owns the DATA-PLANE contracts - `DbError`,
  `DbBinding`, the broker, the read set, and the `ChangeStream` capability trait.

The relay MAY depend on `zeroship-data-core`, and today does not need to. That is
a permission, not a recommendation, and it is specifically **not** a licence to
route the wire types there. Two measured reasons, both of which point the same
way: `crates/zeroship-cdc-wire/Cargo.toml` refuses `zeroship-core` by name -
"a crate whose stated property is no I/O cannot have an HTTP client in its normal
closure and still mean it" - and `zeroship-data-core` reaches `cyper` through
`zeroship-core` (measured with `cargo tree -p zeroship-data-core -e normal`:
`cyper` 3 times, `tokio` 2 times). The other direction is worse: `data-core` is
the floor of the WORKER's data plane, so putting the relay's wire types there
would put the worker's whole data plane into the relay's closure - the exact
coupling the binary shape exists to prevent.

`ChangeEvent` and `ChangeOp` are in `crates/zeroship-core/src/change_event.rs`,
one ring further out than `data-core`, beside `usage_event` and
`replication_names`. Any plan that says "`ChangeEvent` is already in data-core"
is planning against a tree that does not exist.

### The relay's error handling is internal, by construction

`crates/zeroship-data-postgres/src/pg_error.rs` owns `classify`, the translation
from `compio_postgres::Error` into `DbError`. The relay does not share it and
will not. **A binary that nothing links has no shared-vocabulary problem: there
is no consumer to agree with.** That is the whole answer, and it dissolves a
question rather than choosing among its options - see "Why it is this way".

### Cardinality

`Datastore` is one physical PostgreSQL database; `Database` is one creator-owned
schema in it; `Grant` binds an app to one Database; `Cluster` is one physical
PostgreSQL cluster.

- One relay leader per `cluster_id`, elected by session advisory lock.
- Under that leader, **one exact slot, one pgoutput stream and one shared
  publication per Datastore**. A second Datastore adds a stream; it cannot be
  folded into an existing slot.
- Each stream names only its own Datastore's publication, fixed at
  `START_REPLICATION`.
- Creating a Database edits the Datastore's existing publication live and
  restarts nothing. Creating a Datastore provisions an empty publication and
  exact slot, then starts an additional stream.

Names are keyed by the typed `DatastoreId`, never by `app_id`:

```text
datastore_publication_name(id) = "__zs_pub_"  + hex(sha256(id)[0..14])
datastore_slot_name(id)        = "__zs_slot_" + hex(sha256(id)[0..14])
```

The current app-keyed `publication_name(app_id)`
(`crates/zeroship-core/src/replication_names.rs`) is replaced, not aliased, and
every caller changes in the same patch.

Stock `max_replication_slots = 10` and `max_wal_senders = 10` are hard capacity
ceilings and both are `context = postmaster`. Datastore provisioning reserves
both before declaring a Datastore CDC-ready and fails closed when either
reservation is unavailable. The relay refuses a topology snapshot that maps one
Datastore to two clusters, duplicates a slot name, or lacks that reservation.

### The wire projection

**The projection is a PostgreSQL publication column list, computed from the
declared field set by `zeroship-migrate-server`, applied as DDL, enforced by the
server.** Excluded names and excluded bytes never enter pgoutput, so no relay or
worker code can leak them.

```text
wire_columns(collection) =
    replica_identity_columns(collection)
  UNION
    { storage.valueColumn(field)
      : field in declared_fields(collection)
        AND wire_exposure(field) IN { Plaintext, Masked } }
```

`storage.valueColumn` is the descriptor's field-to-physical-column mapping, the
same block `value_column_for_field` reads on the query side
(`crates/zeroship-schema/src/query.rs:3603`). After the shipped storage flip the
logical field column holds the creator-visible value including the mask, and
`storage.rawColumn` names the `__zs_raw__<field>` column holding plaintext or
ciphertext (`crates/zeroship-migrate-core/src/render/gen_types.rs`,
`crates/zeroship-data-engine/src/crud/mask_pass.rs`). `rawColumn` is never in the
wire set because the set is built from `valueColumn` and nothing else.

`wire_exposure` is a closed renderer result, never a guess from a name:
unclassified storage is `Plaintext`; a field with a physical `rawColumn` has a
safe `Masked` `valueColumn`; a field with non-public classification and no
separate safe representation is `ExcludedProtected`. The last case includes the
supported explicit `.mask({ kind: "none", classification: ... })` shape, for
which `raw_column_for_field` deliberately returns none
(`crates/zeroship-migrate-backend/src/schema.rs:599`). If an excluded protected
field is part of replica identity the migration is refused rather than unioning
it back in. Updates visible only through such a field cannot drive incremental
subscriptions; reads and reset refetches still see the authorized value.

`ChangeEvent` (`crates/zeroship-core/src/change_event.rs:10`) changes shape.
`new_tuple: HashMap<String, String>` becomes `new_tuple: ProjectedTuple`, a
newtype with exactly two constructors:

- `ProjectedTuple::from_published_relation(&RelationEntry, &TupleData)` for the
  relay arm, whose guarantee is upstream in the publication;
- `ProjectedTuple::from_descriptor(&Value, raw)` for the SQLite arm, which
  filters against `storage.valueColumn` per declared field. SQLite has no
  publication and today sets `changed_columns: new_tuple.keys().cloned()`
  (`crates/zeroship-data-sqlite/src/cdc.rs:635`).

`changed_columns` is never constructed independently: both producers derive it
from `ProjectedTuple::visible_keys()`. The target `ChangeEvent` **deletes
`old_tuple`**. For an identity-changing UPDATE `pk` is the pre-change
replica-identity vector and the new identity remains in the positional values;
otherwise `pk` is the current identity for INSERT/UPDATE and the deleted
identity for DELETE. The newtype does not create the guarantee; it makes every
producer name which guarantee it relies on, so a third producer cannot rely on
none.

### The DDL bracket

A published column is a catalog dependency, so a migration that drops one aborts
unless publication membership is narrowed first. Three steps, on the pinned
migration connection, while the Database lock is held:

1. **Shrink.** For every table of this Database currently in the publication,
   set its column list to `old_wire INTERSECT new_wire`. A table leaving the
   declared set is dropped from the publication. The published set never grows
   here.
2. **The DDL**, through the existing per-file engine path with
   `LockMode::AlreadyHeld` (`crates/zeroship-migrate-server/src/apply.rs:717`).
3. **Widen.** Set every table in the new declared set to `new_wire`; add tables
   that were not members. The epoch marker is emitted in **this** transaction.

The window between shrink and widen publishes a subset of both sets. **The
bracket can lose a column from a change; it cannot leak one.** That polarity is
why shrink comes first.

Mechanics that "use ADD/DROP instead of SET" hides:

- Changing an existing member's column list is `DROP TABLE` then `ADD TABLE`
  **in one transaction**. `ADD TABLE` alone is `42710`. `ALTER PUBLICATION`
  applies the pair transactionally.
- The drop set is read from the catalog, not computed from the declared set.
  `DROP TABLE` on a non-member is `42704`, on a missing relation `42P01`, and
  there is no `IF EXISTS`. The reconciler reads
  `SELECT tablename, attnames FROM pg_publication_tables WHERE pubname = $1 AND schemaname = $2`
  and diffs. `attnames` is the right column: it follows `RENAME COLUMN` and
  returns zero rows once `DROP COLUMN ... CASCADE` has removed the table.
  `pg_publication_rel.prattrs` is not needed.
- An empty column list is unrepresentable in PostgreSQL. The
  `replica_identity_columns` UNION term is what makes shrink unable to produce
  one. That is load-bearing.
- `RENAME COLUMN` needs no bracket but does need a marker: membership survives
  and `attnames` follows the rename, but the wire name the relay sees changes.

`ALTER PUBLICATION ... SET TABLE`
(`crates/zeroship-migrate-server/src/publication.rs:46`) is deleted. It is a full
replace computed from ONE app's schema, so two tenants sharing a Datastore
publication would each silently remove the other's tables. The existing
`pg_advisory_xact_lock(hashtextextended($1, 0))` on the publication name
(`publication.rs:86`) is kept as the Datastore publication mutex; it serialises a
full replace rather than fixing it, and PostgreSQL advisory locks are
database-scoped so it covers nothing outside that Datastore.

The publication also always contains the reserved
`__zeroship_cdc.heartbeat (id, nonce)` member used for idle confirmation.
Per-Database reconciliation treats it as an invariant system entry: never
returned in a Database diff, never dropped when the last creator table leaves.
This is one narrow, deliberate supersession of the architecture rule that
publication membership excludes the whole `__zeroship_` namespace: no reserved
relation is a member except that exact table with that exact projection.
Provisioning records its relation OID and the decoder requires OID, qualified
name and column shape before treating a change as a heartbeat. Any other
reserved member makes the Datastore unavailable. The frame is consumed by the
relay and never enters an app ring.

The Database advisory lock prevents another migration for this Database from
interleaving the three steps. It does not stop a sibling Database or creator
DML; changes committed between DDL and widen are intentionally decoded through
the intersection. The lock axis is a prerequisite, not current fact: the host
keys it through `ExecutorConfig.project_id`
(`crates/zeroship-migrate-server/src/apply.rs:348`, released at `:442`). The
Database rekey must land before the bracket, or two apps sharing a Database can
interleave it.

### Classification changes take a Datastore reset barrier

Old WAL may still contain plaintext under the same physical field even when its
logical table or field name changed, so a publication intersection cannot remove
it. During its one ordered fold the renderer compares field exposure at the exact
net-applied migration set against the projection that committed. Any existing
field changing from wire-plaintext to `Masked`, `ExcludedProtected` or another
non-plaintext representation enters `newly_protected_fields`.

The fold assigns an internal `ProjectionFieldKey` to every checkpoint field and
carries it through ordered `renameTable` and `renameColumn`; a create receives a
new key and a drop retires one. It also carries every checkpoint wire-plaintext
name through the rename map even after its key retires. A reset is required when
either (a) one provenance key changes from plaintext to protected, or (b) a final
protected qualified name collides with one of those mapped historical plaintext
names. Rule (b) deliberately catches drop-and-create reuse, where pgoutput has no
lineage identifier. False-positive resets on ambiguous rename/drop chains are
accepted; a missed collision strands pre-DDL plaintext in retained WAL.

The barrier is a durable, generation-keyed state machine owned by control:
`Quiesce -> Restore -> Ready`. Before shrink the migration service calls
`PrepareClassifiedMigration(database_id, expected_epoch, apply_attempt_id,
apply_fingerprint)`. The relay takes the exclusive Datastore fence, cancels,
closes and joins the decoder, appends `AppReset(ClassificationChanged)` to every
ring, clears egress, drops the exact slot, proves absence and acknowledges
`Quiesced`. The bracket then runs. On completion the service posts one
idempotent finalize carrying the complete authoritative head vector for the
Datastore; control validates it, persists it, and enters `Restore`. Only that
durable snapshot authorizes the fresh slot; the relay decodes its heartbeat,
seeds observed epoch from the snapshot, acknowledges `RestoreReady`, and control
publishes `Ready`.

This loses every incremental change committed during the barrier and refetches
every app sharing the Datastore, including apps whose schema did not change.
That cost is accepted: keeping the slot would let plaintext already in WAL enter
the relay after the field was classified, which is a confidentiality failure
rather than an availability trade.

### Where the column set comes from

**The migration service never receives a descriptor.** `ApplyMigrationsRequest`
carries `descriptor_sha256`, a deploy-ordering anchor, and `documents:
Vec<IrDocument>`. Nothing can be projected from a digest, and
`publication_membership_sql` takes `tables: &[String]`. The service does hold the
material, because the descriptor and the DDL are folded from the same ordered IR.

The integration makes that one resolved artifact typed and reusable. Each file is
read, deserialized, policy-resolved and canonically serialized exactly once into
a `ResolvedIrDocument` prepared before preflight and passed to both preflight and
apply; `ResolvedMigrationIr` has a private constructor so unresolved ops cannot
enter. The renderer gains a typed output-only projection rather than asking the
host to reverse-engineer `CollectionDescriptor` (which carries declared fields
and indexes but no physical storage mapping and no folded primary key):

```rust
pub struct WireProjection {
    pub relations: BTreeMap<QualifiedRelation, WireRelationProjection>,
    pub newly_protected_fields: BTreeSet<QualifiedField>,
}
pub struct WireRelationProjection {
    pub published_columns: BTreeSet<WireColumnIdentifier>,
    pub replica_identity_columns: BTreeSet<WireColumnIdentifier>,
}
```

`WireColumnIdentifier` and `LogicalFieldIdentifier` have private fields, no
`Display`, and fallible renderer-only constructors; `WireColumnIdentifier` can be
built only from a folded `wire_exposure` result and refuses `rawColumn`
identities and `ExcludedProtected` fields. The publication SQL renderer accepts
that type directly, so a `String` or even a general `SqlIdentifier` cannot be
substituted by the host. `SchemaExport`
(`crates/zeroship-migrate-core/src/render/gen_types.rs:536`) gains
`wire_projection`, built by a new `render_schema_export_resolved(documents,
checkpoint, selection)` during its existing single fold.

Both selection variants are deltas relative to `checkpoint.net_applied`, never a
request-sized replay over `checkpoint.folded_schema`. `PlannedDelta` is the
engine's own preflight classifier output after versioned, repeatable-checksum and
squash semantics. `CommittedDelta` is what the post-run effective journal proves
newly recorded. A step's disposition is load-bearing: on an existing Database
where every superseded migration is already effective, the engine journals a
squash and its supersession edges **without running `up`**
(`crates/zeroship-migrate-core/src/ops/squash.rs`), so that step is
`RecordSupersessionNoOp` and leaves `folded_schema` byte-identical. The engine
exposes the same typed disposition from preflight and from journal recovery, so a
crash cannot turn a record-only squash into executed schema ops.

Two things this forbids: the service must not construct `__zs_raw__<field>` or
any historical `_masked` name, and it must not fetch the descriptor over HTTP
from the build.

### The epoch producer and the head row

No epoch producer and no system schema exist today
(`crates/zeroship-data-engine/src/auth/bootstrap.rs`). Datastore provisioning
creates exactly one reserved table:

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

Owned by an operator no-login role, containing no function. `PUBLIC`, app and
worker roles get nothing. The relay's Datastore management login gets schema
usage and column-level `SELECT` on `database_id`, `physical_schema`, `epoch`,
`last_rotation` and `checkpoint_sha256` only; it cannot read
`projection_checkpoint` or pending claims and cannot write. The migration service
writes the row through its existing privileged provisioning DSN, which is a
superuser DSN today and is not pretended to be least-privilege.

`projection_checkpoint` is the canonical encoding of
`ProjectionCheckpoint { folded_schema, net_applied: BTreeSet<MigrationIdentity> }`
- the exact effective journal set after squash and repeatable semantics, not a
count and not an assumed filename prefix. This is what makes `OnUnmet::Skip`
implementable: skip leaves one migration pending while an unrelated later
migration applies, so "first N documents" is not a valid schema boundary.
`checkpoint_sha256` lets the relay attest the commit tuple without receiving the
folded schema.

T1 writes a typed `MigrationApplyId` into `pending_rotation` with the current
epoch in `pending_base_epoch`, in the same transaction that reaps E-1 roles. T4
clears both atomically with the head advance, checkpoint hash and
`last_rotation`. If the journal does not move and there is no recovery debt, a
no-rotation repair transaction widens back to the exact checkpoint projection and
clears the claim without minting roles, incrementing the epoch, moving
`last_rotation` or emitting a marker. A journal that moves only through
`RecordSupersessionNoOp` uses a distinct journal-only convergence transaction:
new `net_applied` and hash, moved `last_rotation`, no role rotation, no epoch, no
marker.

Whether the engine loop succeeds or stops at its first error, the host retains
that result and runs one convergence tail: reread the exact effective journal,
compute the typed ordered delta, and advance the checkpoint only with migrations
whose exact version and checksum the journal says applied. Earlier committed
identities survive even when they came from the same source document as the
failure. If a newly journaled `ApplyOps` identity has no byte-identical prepared
document, recovery refuses with `ProjectionHistoryUnavailable`, leaves the
publication shrunk and the claim pending, and the creator must retry with the
original bundle. That fail-closed arm is the cost of not storing every migration
body in a second journal.

The post-lock `reconcile_app_publication` call
(`crates/zeroship-migrate-server/src/apply.rs:509`) is deleted. There is one
publication mutation path, not a second repair pass outside the lock.

### The schema-change signal

A relay stamping `(database_id, database_epoch)` from a cached map compares two
things with no common ordering: the WAL position of the change and the wall-clock
moment it read the map. The marker dissolves that.

`pg_logical_emit_message` is emitted **inside the widen transaction**, so "the
publication reached its new shape" and "the database epoch advanced" are one
commit and the relay learns both from the same ordered stream. Not the shrink
transaction: shrink produces a state no descriptor describes. The payload is a
typed wire encoding of `(database_id, database_epoch)` in canonical ASCII, never
an app id inferred from a schema name.

The relay must request `messages: true`
(`libs/compio-postgres/src/replication.rs:889`); the option defaults to false and
a relay that omits it sees no markers **and no error**.

Marker authority is an ACL, not the absence of raw SQL: the WAL `Message` carries
no emitting role. Datastore provisioning revokes both PostgreSQL-18 four-argument
overloads from `PUBLIC`:

```sql
REVOKE EXECUTE ON FUNCTION
  pg_catalog.pg_logical_emit_message(boolean, text, text, boolean) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION
  pg_catalog.pg_logical_emit_message(boolean, text, bytea, boolean) FROM PUBLIC;
```

Worker, app and relay roles get no grant; the bytea overload gets no
non-superuser grant. The explicit fourth argument `false` pins the PostgreSQL-18
identity. There is no PostgreSQL-16 signature branch. The first argument is
always `true`; non-transactional messages are outside the contract.

The marker is a *guarded* property, not an unrepresentable one - it is one
function in one privileged service. The bracket narrows it: a widen that does not
run leaves the publication measurably shrunk, so a missing marker has a second
independent observable rather than being pure silence.

### What moves, is deleted, is retained

The crate is `crates/zeroship-data-cdc-server` (binary; it also ships a lib, for
the one reason `src/lib.rs` states - a `platform` binary must publish its
configuration to a tool that LINKS it, so "ships no lib" is not available as an
enforcement mechanism). Plus the leaf wire crate `zeroship-cdc-wire`. **Nothing
depends on the relay**, and the relay does not depend on `zeroship-plugin-db`
either - neither edge exists, and the manifest states why for each absent one.

**THE CRATE EXISTS AND THE TABLE BELOW HAS NOT HAPPENED.** `545ceff1e` created
it with ZERO files moved. Read every verdict here as END STATE, not as a
description of the tree; the "today" column says what is true now. The two
columns disagree for four of the eight rows, and that disagreement is the point -
a reader who takes "Deleted" as an instruction will delete a file with a live
production caller.

| file | end-state verdict | today |
| --- | --- | --- |
| `wal_consumer.rs` | Split and rewrite; **do not move the file.** Extract the pgoutput decode algorithm, `RelationEntry` and its primary-key index, the replication parameter check, backoff policy and fatal classification behind relay and wire-owned types, with a `zeroship-cdc-wire` frame emit replacing `broker::publish`. | in `zeroship-plugin-db`, unchanged |
| `replication.rs` | Split and rewrite. Extract exact-slot lifecycle and health-query algorithms behind relay errors. Replace app-keyed names with the two Datastore functions; delete `worker_slot_name` and per-worker drop entry points. `drop_datastore_slot` derives and verifies one exact name plus `database = current_database()` and is reachable only through the fenced reset handshake. Broad prefix enumeration stays forbidden. **The watchdog half and the drop family do NOT go**: `watchdog_query` has a live V8 caller (`crates/zeroship-plugin-db/src/v8_classes/replication.rs`, reached from JS as `env.db.__platform.replication`) and the drop family is called from `service.rs`'s `deprovision_app` as well as from CDC. | in `zeroship-plugin-db`, unchanged |
| `slot_reaper.rs` | **Deleted.** With O(Datastores) service-owned slots and one owner per cluster there is no per-worker slot to abandon. Deleted WHOLE, not split - see "The reaper is a privilege change". | in `zeroship-plugin-db`, unchanged, and still the ONE CDC module that crate exports unconditionally (`crates/zeroship-plugin-db/src/lib.rs:375`) |
| `change_stream_pg.rs` | **Deleted - and this is an END-STATE verdict that becomes reachable only after `RunningConsumer::Postgres` is a handle on a relay subscription.** `SharedExit` (`:31`) and `WalConsumerHandle` (`:72`) supervise a task that no longer exists in the worker; `spawn_consumer` (`:170`) and `deprovision` (`:162`) go with the per-worker slot. `pause_broker` / `engage_schema_pending` survive as the broker functions they already delegate to. | **STAYS in `zeroship-plugin-db`**, and it is not a relay candidate at any point: `impl ChangeStream for PgChangeStream` (`:145`) implements a data-core capability trait whose SQLite peer (`crates/zeroship-data-sqlite/src/cdc.rs:854`) lives in a vendor LIBRARY crate, and it holds `backend: Rc<PostgresBackend>` (`:123`) - an `Rc` is not `Send`, so the type is pinned to the isolate thread, never mind the process |
| `broker.rs` | **Stays.** In-process routing table; consumers are V8 subscription wrappers on the same thread. Keep `message_to_json` (`:1020`) and `ws_frame` (`:1093`); both emit creator-visible names and no row values. | in `crates/zeroship-data-core/src/broker.rs`. **This row said "Stays in `zeroship-plugin-db`" until 2026-09-03**; the module sank into data-core with `read_set.rs` |
| `read_set.rs` | **Stays.** Capture happens inside `ctx.db.find` in the isolate and cannot leave the process. | in `crates/zeroship-data-core/src/read_set.rs` |
| `cdc_lifecycle.rs` | **Stays**, reshaped. The refcounted per-app lease (`acquire` `:87`, `release` `:113`) still decides when this worker needs a stream. Add an atomic snapshot accessor returning `cluster_id`, `database_id`, `database_epoch` and `grant_generation` for every leased app; `acquire` currently mutates a private map one app at a time. `RunningConsumer::Postgres` (`:25`) becomes a handle on the relay subscription. | in `zeroship-plugin-db`, declared `mod cdc_lifecycle;` - plain private, so its external consumer count is structurally zero |
| `exec.rs` emit path | **Deleted outright, and it is the POSTGRES path.** | live in `crates/zeroship-data-engine/src/exec.rs` |

**Why zero files moved, stated as the mechanism rather than as a preference.**
`crates/zeroship-plugin-db/src/wal_consumer.rs` imports `SuppressGuard`,
`has_subscribers` and `publish` from the broker, and all three target
PROCESS-WIDE `LazyLock<Mutex<..>>` statics in
`crates/zeroship-data-core/src/broker.rs`. Move that file into a binary that
links data-core and every one of them resolves to a DIFFERENT process's static:
`publish` reaches zero subscribers and the suppression guard suppresses nothing
in the worker, so the worker's local-emit fast path keeps emitting while the
relay believes it has taken authority. **Nothing fails to compile and no gate in
this tree sees it.** That is why the verdict for the file is "split and rewrite",
and why the crate that exists today contains no decode loop rather than a moved
one.

This deduction rests on `LazyLock` being per-linked-binary and has NOT been bound
by a two-process harness - the harness cannot be written until the relay exists.
Treat it as near-certain and unproven, in that order.

The local-emit path serves Postgres, not SQLite:
`backend_publishes_committed_changes()` is true on SQLite and `emit_for_rows`
returns early on it (`crates/zeroship-data-engine/src/exec.rs:445`), because SQLite
builds its `ChangeEvent` in `crates/zeroship-data-sqlite/src/cdc.rs` and
publishes straight to the broker. So the delete is `emit_for_rows`,
`queue_or_emit`, `drain_pending_emits_on_commit` and `clear_pending_emits`;
`exec_mutation_with_emit` (`exec.rs:370`) collapses to `exec_mutation`;
`SUPPRESSED_APPS` (`broker.rs:771`), the suppression guard and `emit_local`
(`broker.rs:850`); and the `pending_emits` slot on the isolate context with its
transaction-cleanup call and test helpers. That last item is why the scope must
be read carefully: this is not a three-function edit inside `exec.rs`.

Deleting it is not optional. It is a second producer of the same event with a
*different* projection - a two-name blacklist against the publication whitelist,
running before the mask pass, against a stream the server has already projected.
One producer per backend.

`ChangeStream` (`crates/zeroship-data-core/src/storage.rs:409`) survives as a
capability trait with a changed Postgres implementation: `spawn_consumer` becomes
"subscribe to the relay"; `deprovision` removes only this worker's local lease
and performs the make-before-break response replacement without the app. It
cannot delete a shared relay ring or revoke a Grant.

`zeroship-plugin-db` keeps the whole data plane, the in-process broker and its
read-set narrowing, the CDC lease bookkeeping, and the SQLite CDC publisher. It
loses every line that speaks the streaming replication protocol, and with it the
reason its process needs `REPLICATION`.

### The reaper is a privilege change, and it gets its own commit

**Extracting the crate moved no privilege, and that is the deliberate outcome
rather than an omission.** `git diff --name-only 8672dd355..HEAD --
crates/zeroship-worker crates/zeroship-plugin-db db/migrations-ts` returns zero
files. The worker still holds `REPLICATION` and `BYPASSRLS`.

`db/migrations-ts/20260818000200_worker_database_authority.ts:35` grants
`zeroship_worker` both, and `crates/zeroship-worker/src/db_posture.rs:123`
REFUSES TO BOOT on `if !posture.replication || !posture.bypass_rls`. **This
document cited `:125` until 2026-09-03**; `:125` is inside the message, not the
predicate. So deleting the worker's import without dropping the two role
attributes leaves the worker holding `REPLICATION` with nothing using it - the
code moves and the privilege does not, which is exactly the outcome AGENTS.md's
"Privilege follows the PROCESS, not the function" warns this extraction can end
in.

Four edits land together, in ONE commit whose body says what privilege moved
where. None of them is correct alone:

1. The same migration that provisions the relay's streaming role DROPS
   `REPLICATION` and `BYPASSRLS` from `zeroship_worker`. The relay's streaming
   login takes only `REPLICATION`, never `BYPASSRLS`, and never issues
   creator-table SQL.
2. `crates/zeroship-worker/src/db_posture.rs:123` INVERTS: it must require the
   ABSENCE of both, so a worker that somehow still has them refuses to boot.
3. `crates/zeroship-worker/src/slot_reaper.rs` is deleted, with its module
   declaration, its startup call and its supervision arm. `main.rs` carries a
   test asserting production main supervises exactly one process-wide reaper; it
   is deleted in the same change or it fails, and "delete the failing test" must
   not be done ahead of the rest.
4. `crates/zeroship-plugin-db/src/slot_reaper.rs` is deleted, with
   `pub mod slot_reaper;` at `crates/zeroship-plugin-db/src/lib.rs:375`. That is
   the ONE CDC module plugin-db exports unconditionally - `change_stream_pg`,
   `replication` and `wal_consumer` are `pub(crate)` unless `test-helpers`, and
   `cdc_lifecycle` is plain private - so deleting it takes plugin-db's external
   CDC surface to zero.

**The file is deleted WHOLE, not split.** Its two halves are welded: the sweep
decision reads `worker_token == self.own_worker_token || slots.iter().any(|slot|
slot.active)`, and every other worker's liveness is decided by
`try_acquire_worker_lease`. The lease is the INPUT to the reap decision, not an
adjacent concern that shares a file. Splitting it manufactures a cross-process
lease-key agreement whose failure mode is "reap a live worker's slot", for a
mechanism this design removes; moving both halves into the relay while per-worker
slots still exist is a live-slot-loss regression, because a worker in replication
reconnect backoff has an inactive slot and an unchanged fingerprint.

**The entire call-site surface is one import**:
`crates/zeroship-worker/src/slot_reaper.rs:7` takes `OperatorSlotReaper`,
`ABANDONED_INACTIVITY_THRESHOLD` and `SWEEP_INTERVAL` from
`zeroship_plugin_db::slot_reaper`. Every other cross-crate `zeroship_plugin_db::`
reference outside the crate is `service::*`.

### The worker/relay wire contract

`zeroship-stream` is not the carrier, for four reasons read out of its own
source, not taste:

- **A consumer group partitions; live-query fan-out broadcasts.**
  `crates/zeroship-stream/src/transport.rs` is an explicit Kafka consumer-group
  contract and the Redpanda adapter subscribes with a `group.id`. One shared
  group delivers each event to exactly one worker; one group per worker makes
  every worker ingest every tenant's feed.
- **`poll` takes no topic** (`transport.rs:48`); the topic is fixed at
  construction, so a per-app topic means a per-app librdkafka producer, consumer
  and C thread set.
- **`publish` is a blocking spin with one broker round trip per record**
  (`adapters/redpanda.rs:297-308`, `acks=all` at `:231`). A synchronous stall on
  a compio thread, per row, on the write-notification path.
- **The transport is plaintext and unauthenticated by construction**
  (`adapters/redpanda.rs:108-146`): no TLS, no SASL, and the workspace pins
  rdkafka without those features. Today's payload is usage counters; CDC payloads
  are creator rows, which is a different sensitivity class.

`zeroship-stream` remains right for the durable usage/billing outbox it exists
for. Nothing here changes it.

**Encoding. SHIPPED at `05d9462c8`** as the leaf crate `zeroship-cdc-wire`, no
I/O, no V8, to be depended on by both `zeroship-data-cdc-server` and
`zeroship-plugin-db` (**this line named `zeroship-cdc` until 2026-09-03**).
Neither consumer names it yet: 35 tests, zero dependents. Its normal dependencies
are `sha2`, `serde` and `serde_json`; `zeroship-core` is a DEV dependency only, so
a differential test proves this crate's base62 parser accepts exactly what
`zeroship_core::typed_id` accepts without putting an HTTP client in a no-I/O
crate's shipped closure. Each frame is
`u32_be(frame_len) || u8(tag) || payload`, where `frame_len` counts the tag and
payload. Integers are big-endian; byte and UTF-8 strings carry a `u32_be` length;
enum discriminants are `u8`. A zero length, a length over `max_frame_bytes`,
invalid UTF-8, trailing payload bytes or an unknown tag is a fatal protocol
violation that closes the response. The request and `Hello` name the one exact
supported handshake version; there is no best-effort decode of another.
`max_frame_bytes = 1_048_576` and `max_cell_bytes = 524_288` are protocol
constants, not per-relay tunables, so every encoder makes the same
`Gap(OversizeValue)` decision.

The primitive layout is closed. `wire_version` is `u16`; `timeline`,
`change_index` and vector counts are `u32`; `systemid`, LSNs, terms, epochs,
generations, sequences and `prime_batch_id` are `u64`. An LSN is its raw
`XLogPtr` integer, not `0/16B6C50` text. A typed id is its canonical ASCII
rendering, bounded to 64 bytes and parsed by the expected concrete id type; a
wrong prefix or noncanonical base62 form is fatal. `Option<T>` is `u8(0)` or
`u8(1) || T`. Structs concatenate fields in declaration order with no field
numbers or padding. `0x00` and `0x01` are the only Boolean encodings. Counts,
lengths and multiplication are checked before allocation. `LeaderTerm`,
`DatabaseEpoch` and `GrantGeneration` encode as `u64` but their constructors
admit only `1..=i64::MAX`, matching the `bigint` columns that persist them; a
decoder refuses an out-of-domain value before comparing or storing it. Not
`serde_json`: a JSON `HashMap<String,String>` per row is exactly what let a bag
of physical column names become the thing on the wire.

`wire_version = 1`. Frame tags are fixed: `Hello=0x01`, `RelationPrime=0x02`,
`Registered=0x03`, `Resync=0x04`, `AppReset=0x05`, `Relation=0x06`,
`Change=0x07`, `Gap=0x08`, `Truncate=0x09`, `Epoch=0x0a`, `Heartbeat=0x0b`.
Nested enums are fixed too, and any other discriminant is a fatal v1 decode error
rather than an `Unknown` variant.

| frame | payload |
| --- | --- |
| `Hello` | wire version, relay id, leader term, `cluster_id`, `systemid`, `timeline` |
| `RelationPrime` | unsequenced: `(prime_batch_id, app_id, database_id, database_epoch, grant_generation, relation_generation, collection, columns)` |
| `Registered` | registration generation plus one app-identified outcome per requested app, in request order |
| `Resync` | unsequenced, connection-local `(prime_batch_id, prime_relation_count, app_id, database_id, database_epoch, grant_generation, ResetReason, accepted_cursor)` |
| `AppReset` | sequenced app-wide `(ResetReason)` |
| `Relation` | sequenced `(relation_generation, collection, columns)` |
| `Change` | sequenced `(relation_generation, op, commit_lsn, change_index, pk, values)`, positional against the primed or last `Relation` |
| `Gap` | sequenced `(collection, pk, commit_lsn, change_index, reason)` |
| `Truncate` | sequenced `(collections)` |
| `Epoch` | sequenced `(database_epoch)` |
| `Heartbeat` | `(datastore_id, last_confirmed_lsn)` - Datastore health telemetry only, never a worker resume coordinate |

The framing borrows pgoutput's own solution to the same problem: column names
must not repeat per row. A `Change` costs its values plus a small header. A cell
over `max_cell_bytes`, or a `Change` over `max_frame_bytes`, becomes a keyed
`Gap`; a `Gap` that cannot fit triggers `AppReset(AppDegraded)` and quarantine.
The migration projection builder refuses a relation whose encoded `Relation` can
exceed `max_frame_bytes`, so an accepted schema cannot discover that failure
while streaming.

`CellValue` is three-way - `Value(Bytes) | Null | Unavailable` - not
`Option<Bytes>`, because pgoutput sends an unchanged-TOAST marker in place of a
large unmodified value. **Encoding "the server did not send it" the same way as
"it is NULL" is the same class of collapse the projection exists to prevent.**
`Gap.reason` is a closed enum (`OversizeValue`, `UnrepresentableType`) rather
than a string, because an open reason field is where a physical column name
reappears the moment someone writes a helpful error message.

Every sequenced frame carries the envelope `(app_id, grant_generation, seq)` and
lives in the one ring for that exact binding. `Registered` and `Resync` are
connection-local and consume no app sequence: with two workers on one app,
putting one slow worker's reset in the shared sequence would either reset the
healthy worker or leave a hole in its cursor. `AppReset` is shared because it
records a condition invalidating every worker for the app.

`relation_generation` is a monotonic `u64` never reused within
`(relay_id, leader_term, app_id, grant_generation)`, including across a same-term
Datastore reconnect or slot recreation. Losing that counter is loss of sequence
state and forces self-demotion.

**Transport.** `POST /internal/v1/cdc/subscribe` with
`Content-Type`/`Accept: application/vnd.zeroship.cdc.v1`, body one binary
`SubscribeRequest` capped at 1 MiB and 1,024 apps. A success is `200` whose first
frame is always exactly one `Hello`, followed by primes and `Registered`, after
which the worker reads frames until the body closes. Server is `ntex` v3 with
`compio` and `rustls` (the workspace enables only `compio` today, so `rustls` is
an explicit relay-crate feature addition); client is `cyper` with its `stream`
feature. Push rather than poll, because a poll interval is a latency floor chosen
in advance.

**That pair is PROVEN, at `f7e043252`**, as twelve tests in
`crates/zeroship-cdc-transport-spike/` - incremental in both directions, on
compio with no tokio reactor, ntex write backpressure reaching the response body,
and "the relay sheds, it never blocks" expressible as written with a
one-variable control. What is still unproven is TLS terminating in the relay
process (the spike is plaintext loopback and the rustls feature is not enabled
today) and any hold longer than seconds. See the Status block for the numbers and
for what the spike deliberately does not establish.

Authentication is a single-use service assertion with exact audience
`spiffe://zeroship.ai/svc/cdc`
(`crates/zeroship-core/src/service_assertion.rs`). The relay verifier uses
`PostgresReplayStore` (`crates/zeroship-authn/src/service_replay.rs`) against the
shared `service_authn.service_assertion_replay` table in the coordination
database, **never** `InMemoryReplayStore`, which is limited to a single-replica
callee while leader failover makes the relay replicated. Failure to claim a `jti`
refuses the subscription before its body is parsed. Coordination PostgreSQL
availability is therefore also an authentication dependency.

Every `cdc_endpoint` is `https://` and TLS terminates in the relay process, not
at a load balancer followed by a plaintext hop. The worker's rustls client
verifies the operator CA from a required bundle and the endpoint DNS name; plain
HTTP, disabled validation and an IP absent from the certificate are refusals.
Adding the bundle to host-global roots is forbidden.

HTTP failures use `application/problem+json` with only a closed `code`:
`401 AuthenticationFailed`; `400 DuplicateApp`,
`NonMonotonicRegistrationGeneration`, `ConflictingShard`, `MalformedRequest`;
`406 NotAcceptable`; `409 ClusterMismatch`; `413 RequestTooLarge`;
`415 UnsupportedMediaType`; `426 UnsupportedWireVersion` plus a version header;
`503 NotLeader`, `AuthenticationStoreUnavailable`, `TermAuthorityUnavailable`,
`ConnectionCapacityExceeded`. None begins a binary body or emits `Hello`. The
four `503`s retry with full jitter from 250 ms to 5 s, minting a fresh assertion
each attempt. Other HTTP errors are terminal until topology or code changes and
are never converted into an empty successful registration. A frame decode
violation closes the response, drops staged state, surfaces the closed decoder
code without row bytes, and does not retry the same endpoint and wire version
until topology revision or process version changes, so an invalid peer cannot
create a hot reconnect loop.

**Registration.** The worker groups leases by `cluster_id`, sorts by canonical
`app_id` bytes and splits into deterministic contiguous shards of at most 1,024
apps. One immutable request per shard; apps from two clusters never share a
response. All shards of one snapshot share a `registration_generation` and carry
`shard_index` and `shard_count`. `SubscribeRequest` carries `wire_version`,
`cluster_id`, `worker_id`, a control-signed `term_permit`, the generation, both
shard fields, and per app an `expected_binding {database_id, database_epoch,
grant_generation}` plus an optional `AppCursor`.

The permit commitment is SHA-256 over domain `zs-cdc-subscribe-v1\0` plus the
canonical request bytes with `term_permit` encoded as zero-length; neither the
permit nor the `Authorization` header is in its preimage. The relay rejects a
foreign `cluster_id`, then authenticates the assertion and redeems the permit,
**before examining any app id**. The permit binds cluster, worker-process id,
credential, admission epoch, relay pair, shard key and commitment, and the
authenticated credential id must equal the permit's.

`worker_id` and `relay_id` are typed `wrk_`/`rly_` UUIDv7 process identities
minted at start; neither survives a restart. Admission is keyed by
`(worker_id, registration_generation, shard_index)` with one fixed `shard_count`
per generation, and "identical request" means the binary body bytes only. A
generation below the highest admitted is always
`NonMonotonicRegistrationGeneration` and disturbs no response. `DuplicateApp` or
`ConflictingShard` poisons the whole candidate generation with a term-scoped
tombstone; the worker does not mint another until its active-set snapshot or
process version changes, because blindly regenerating the same malformed
partition is a hot loop.

The relay makes exactly one resume decision per app, in this order, and a later
rule never hides an earlier failure:

1. Reject an app absent from topology, without an active Grant, or whose
   Datastore stream is unavailable.
2. Expected Database or Grant generation differing from the worker-visible
   serving binding is `StaleExpectedBinding`, not a reset.
3. If desired, observed and serving Database epochs are not all equal, reject as
   `EpochPending`. **This precedes expected-epoch comparison**, so a worker
   correctly carrying the still-serving E gets `EpochPending` while desired E+1
   is hidden behind the barrier.
4. Expected epoch differing from serving epoch is `StaleExpectedBinding`.
5. A cursor whose `app_id` or `worker_id` differs from the request is
   `CursorBindingMismatch`.
6. No cursor resets as `Initial`.
7. A cursor Database or Grant generation mismatch resets as `GrantRebound`; an
   epoch mismatch as `DatabaseEpochChanged`. **Binding comparison precedes term
   comparison**, so a simultaneous rebind and failover cannot retain a watermark
   from the old Database.
8. A different `relay_id` or `leader_term` resets as `RelayFailover`.
9. `next_seq` above `ring_next_seq` is `CursorAhead`; below `tail_seq` resets as
   `RingOverrun`; in range resumes.

The retained interval is `[tail_seq, ring_next_seq)`, empty when equal. Every
reset skips it and accepts exactly the current `ring_next_seq`. One per-app
rejection does not close accepted apps. `EpochPending` and `DatastoreUnavailable`
retry with backoff; `StaleExpectedBinding` triggers one immediate topology
refresh first; `AppNotInTopology` and `GrantInactive` retry only when the lease
set changes; `CursorAhead` and `CursorBindingMismatch` are local invariant
failures and do not spin.

Before acknowledging an accepted app the relay emits `RelationPrime` for every
relation generation referenced by the replay interval **and every currently
active generation**, under a connection-monotonic never-reused `prime_batch_id`,
then one terminal `Registered`. The worker stages primes by
`(response_instance, app_id, prime_batch_id)` and requires exactly
`prime_relation_count` distinct generations; count zero is a valid complete empty
batch. On `Resumed` it atomically replaces the relation cache and replays; on
`Reset` it first clears old relation and live-query state, installs the staged
batch atomically, applies the watermark rule, publishes one local broker
`Resync`, and only then accepts data at `accepted_cursor`. So a reset cannot
discard the primes sent immediately before it. There is no reverse acknowledgment
channel and no hidden client-to-server update channel; a response is never
mutated in place.

**Lease changes are make-before-break within one cluster.** There is no global
all-shards activation barrier: each shard may stream immediately after its own
`Registered`, and the worker atomically selects the new response per accepted app
in that shard. A rejected app keeps its old response. Every data frame is fenced
against the app's selected `(registration_generation, shard_index,
response_instance)` before broker delivery, so a late old-response frame is
discarded even if its `seq` would pass. An old response closes only when no app
still selects it. If a new generation gets `ConnectionCapacityExceeded` because
old responses hold the egress permits, the worker closes the minimum old
responses needed and marks their apps reconnecting - the one explicit
break-before-make fallback.

`ensure_ready` (`crates/zeroship-plugin-db/src/cdc_lifecycle.rs:236`) currently
waits only for a locally spawned consumer. It now completes for one app only
after that app's priming, `Registered` outcome and any required `Resync` have
been processed by the broker. A slow sibling shard is not part of its readiness.

`cdc_lifecycle` builds `apps` solely from its server-injected active lease map.
No V8 value, request field or creator header can supply or replace an `app_id`,
binding or cursor, and the relay independently requires the current active Grant
before returning rows.

### Delete, and "row left the view", without a before-image

`REPLICA IDENTITY FULL` is ruled out, so a DELETE carries only replica-identity
columns and pgoutput supplies an UPDATE old tuple only when those columns
changed. The relay uses it to recover the identity and discards it. Wire `Change`
and the target `ChangeEvent` carry no old tuple.

The broker's current answer tests the predicate against both tuples. The
replacement: the subscription keeps the bounded set of pks it has delivered into
the subscriber's current view. An event whose pk is in that set is delivered
regardless of whether the new tuple matches the predicate, so the subscriber
learns the row left. For an identity-changing UPDATE it removes the old pk and
evaluates the new identity from positional values. The set is bounded by the
query's page size; overflow emits `Resync`. **This is not prototyped**, and it is
the one place where a subscriber-side contract changes shape rather than moving.

### Slow consumers: the relay sheds, it never blocks

**The invariant: a consumer must never be able to reach the slot.** The ring
writer advances `highest_gap_covered_lsn`, and that bound confirms the slot. A
ring writer a subscriber can block is a subscriber that can stop LSN
confirmation, and a slot that stops confirming grows WAL for every tenant on the
cluster.

- **The ring writer's `push` is not `async` and takes `&self`.** That is the
  seam: if it ever needs awaiting, the compiler says so at every call site.
- **Each subscribing connection owns a read cursor into the app's ring plus its
  own bounded egress buffer.** Fan-out is the connection tasks reading their
  cursors, not the writer walking a subscriber list, so writer cost per frame is
  O(1) in subscriber count.
- **A cursor below the ring tail gets connection-local `Resync(RingOverrun)` and
  jumps to `ring_next_seq`.** No shared sequence is consumed, healthy workers are
  untouched, and it is not disconnected: disconnecting makes the worker
  re-register and re-lease, which is more work under load.
- **A connection whose egress buffer has been full longer than
  `slow_consumer_timeout` is CLOSED.** A socket the peer is not reading is relay
  memory the ring needs. A full socket does not itself prove ring overrun.
- **No credit scheme.** Credit is flow control, and flow control here means a
  consumer can slow the producer. The consumer's only legitimate signal is "I
  fell behind", and `Resync` carries it.

The in-process broker already solves this shape and the relay must not regress on
it: `Subscription::push` is not `async` and never awaits; on overflow it clears
the queue and pushes ONE `Resync`, with a `resync_pending` flag collapsing
successive overflows so a wedged subscriber cannot make the publisher do work
proportional to how wedged it is (`DEFAULT_QUEUE_DEPTH = 1024` at
`crates/zeroship-data-core/src/broker.rs:72`, `MAX_SUBSCRIPTIONS_PER_APP = 256`
at `:79`).

The honest cost: a merely-slow worker reconnects repeatedly and gets `Resync`
storms, each of which is a full refetch on the live-query path. A slow subscriber
becomes connection churn and eventually database read load. Accepted, because
read load is bounded by the app's own spend limit and WAL growth is not.

**Ten operator knobs with mandatory defaults**, provisional launch values chosen
to bound failure rather than claimed as workload measurements:

| knob | default | bound |
| --- | --- | --- |
| `ring_bytes_per_app` | 8 MiB | >= `2 * max_frame_bytes`, <= `ring_bytes_total` |
| `ring_frames_per_app` | 16,384 | >= 2 |
| `ring_retention` | 60 s | >= 1 s |
| `ring_bytes_total` | min(512 MiB, 20% of cgroup limit) | >= `ring_bytes_per_app` |
| `egress_bytes_per_connection` | 2 MiB | >= `max_frame_bytes + 4` plus queue metadata |
| `egress_bytes_total` | min(128 MiB, 5% of cgroup limit) | >= `egress_bytes_per_connection` |
| `slow_consumer_timeout` | 15 s | >= 1 s |
| `app_fault_threshold` | 5 faults | >= 2 |
| `app_fault_window` | 60 s | >= 10 s |
| `quarantine_duration` | 300 s | >= one storm window |

Startup requires `ring_bytes_total + egress_bytes_total <= 25%` of the detected
cgroup memory limit. Before sending `200` a subscription acquires one full
`egress_bytes_per_connection` permit from a global byte semaphore; without one it
gets `503 ConnectionCapacityExceeded` and no queue is allocated. The hard
concurrent-response ceiling is therefore
`floor(egress_bytes_total / egress_bytes_per_connection)`.

Eviction is always oldest-first within one app's ring and resets each cursor
below the new tail independently. **There is no drop-the-newest arm**: the newest
frame is the one a live query most needs, and discarding it converts a burst into
a permanently stale view rather than a refetch. What forbids simply making the
ring large is that RSS is the resource one tenant can exhaust for all of them.

### Leader election, terms and fences

PostgreSQL advisory locks are database-scoped, so every contender takes the lock
in one designated coordination database - the `zeroship` database on the control
plane, reached through a dedicated `ZEROSHIP_CDC_COORDINATION_DATABASE_URL` and a
`zeroship_cdc_coord` login that receives exactly: `CONNECT`, `USAGE ON SCHEMA
control`, column-level `SELECT` on the cluster registry, `UPDATE (leader_term,
leader_relay_id)` on it, `USAGE ON SCHEMA service_authn`, full DML on
`service_authn.service_assertion_replay` (the exact privileges the existing
atomic claim-and-sweep needs), and `REVOKE CREATE ON SCHEMA service_authn`.
Reusing any Datastore replication or management DSN for election is forbidden.

```text
cdc_cluster {
  cluster_id primary key,
  advisory_lock_key int4 unique not null,
  postgres_system_identifier numeric(20,0) unique not null,
  postgres_timeline bigint not null check (> 0 and <= 4294967295),
  leader_term bigint not null check (> 0),
  leader_relay_id text,
  predecessor_fenced_term bigint not null check (> 0 and <= leader_term),
  topology_revision bigint not null
}
```

`CDC_LOCK_NAMESPACE` is the fixed signed-int4 `1_515_406_148` (ASCII "ZSCD"). The
second key is allocated and uniqueness-constrained; **hashing `cluster_id` into a
lock key is rejected because a collision would make two clusters exclude each
other.** After `pg_try_advisory_lock` succeeds the contender atomically
increments `leader_term` on the same session under the predicate
`predecessor_fenced_term = leader_term`, so a standby cannot skip an unfinished
fence. An operator-owned `SECURITY DEFINER` row trigger with a no-login owner,
fixed `search_path` and `PUBLIC` execute revoked freezes every active old-term
member against the new relay pair with a random challenge in that transaction,
and it alone advances `predecessor_fenced_term` when the set is empty. The relay
role cannot invoke it or edit fence state. No Datastore stream opens before that
commit. Term exhaustion is a hard operator refusal, never a wrapping increment.

Provisioning obtains `(postgres_system_identifier, postgres_timeline)` from
`IDENTIFY_SYSTEM` (`libs/compio-postgres/src/replication.rs:518`,
`:842`). Every later Datastore DSN must return that exact pair before joining the
cluster. The unique constraint prevents two `cluster_id` values naming one
physical cluster; the per-Datastore check prevents one `cluster_id` spanning two.
A mismatch is a hard configuration refusal, never a warning.

**The connection probe accelerates self-demotion; the predecessor ACK is the
delivery fence.** The coordination session lives for the whole term and carries
only the lock, the term update, and a leader-pair probe every two seconds with a
three-second deadline. EOF, timeout, query error, ambiguous cancellation, a
missing row, or a different returned pair atomically trips one leadership
cancellation token, checked immediately before every confirmation, ring write,
topology acknowledgment and worker-frame send. The process must reacquire the
lock and mint a higher term before serving again. The central control database is
therefore an accepted CDC availability dependency: uncertainty self-demotes
rather than risking split brain.

Control owns one durable `CdcWorkerTermMember` per `(cluster_id, worker_id)` with
credential id, monotonic nonzero `admission_epoch`, active/fenced terms, pending
predecessor/successor pair and challenge, and completed receipt; plus
`CdcWorkerGrantFence` rows keyed by `(cluster_id, worker_id, revision, app_id,
old_grant_generation)`. Workers poll for term permits every two seconds per shard
with nonce, request commitment and `expected_admission_epoch`. Every later
worker-originated membership mutation locks cluster before member and compares
that epoch; a mismatch is side-effect-free `StaleAdmissionEpoch`. For release or
fence completion, exact durable receipt lookup precedes the comparison so replay
returns its recorded epoch without reapplying.

A term change or a Grant revoke/rebind freezes every active member against its
exact successor and challenge. The frozen worker takes one cluster-exclusive
guard, drains delivery, cancels named responses, purges queues, records per-app
minimum Grant generations, and sends exact ACKs. Control exposes revoke/rebind
only after all frozen-worker receipts **and** an exact current-pair relay
topology ACK; a term change invalidates older relay evidence, so the successor
must ACK even when worker receipts exist. **There is no timeout**: a paused
worker blocks until it drains or a supervisor proves that exact process dead and
its credential revoked, using its termination record. Neither relay may
manufacture the death proof.

The worker keeps one term fence per `cluster_id`; every response frame and broker
item retains its exact `(cluster_id, relay_id, leader_term)`. Broker admission
and subscriber delivery take that fence's shared guard through one non-yielding
side effect. A valid higher pair from control or a verified `Hello` makes the
worker acquire exclusive, wait for every in-flight guard, cancel lower-term
responses, purge their queues, publish local `Resync(RelayFailover)`, and advance
the pair **before** acknowledging control. A paused guard therefore prevents the
ACK instead of resuming after it. Lower terms close; same-term with a different
relay is split brain; another cluster's responses are untouched.

**Topology channel.** The leader long-polls
`GET /internal/v1/cdc/clusters/{cluster_id}/topology?after_revision=R` with a
single-use assertion for the control audience. Control returns either no change
or one complete typed snapshot; it never returns a patch. The relay validates the
whole snapshot and its foreign keys before atomically replacing revision R, then
posts a `TopologyAck` naming its relay pair and one status per Datastore in that
revision. Per Datastore the status is `Ready | Quiesced | RestoreReady |
Unavailable`, and `Ready`/`RestoreReady` carry every Database and Grant for that
Datastore exactly once, sorted, with no foreign child. `Quiesced` and
`Unavailable` require empty child vectors because neither has a live decoder;
`Quiesced` attests that exclusive acquisition drained earlier writers and proved
the exact slot absent, `RestoreReady` additionally attests the relay holds
exclusive. `Unavailable` never satisfies a barrier.

Control accepts the ACK in one transaction that row-locks the same cluster row
used by term increment, requiring the exact `(leader_relay_id, leader_term)`,
`predecessor_fenced_term == leader_term`, and a revision no greater than the
latest. Either an N+2 term update precedes the check and rejects N+1, or the N+1
ACK commits before N+2 can become current. **ACKs are cumulative
complete-snapshot evidence, not one global pending latch**: per Datastore control
stores the greatest acknowledged revision, its relay pair, and the exact observed
reset, Grant and epoch generations, and an ACK at R satisfies a barrier created
at B only when `R >= B`. **Barriers serialize per Datastore, not per cluster**, so
a revision blocked on failed Datastore A can coexist with a later one completing
for healthy B.

`cdc_endpoint` is a cluster-local load-balanced address whose readiness is green
only while the leadership guard and topology loop are live. Datastore readiness
is reported separately: a failed Datastore produces
`Rejected(DatastoreUnavailable)` only for its apps. Making global readiness
depend on every stream is rejected, because one broken tenant database would
remove the only lock holder from the load balancer while preventing a standby
from taking over. A non-leader answers `503 NotLeader` with no redirect URL.

Each Datastore topology entry names a separate `management_dsn_secret_ref`. That
login may read `pg_replication_slots` and `pg_stat_activity`, read only the five
granted head columns, update only `__zeroship_cdc.heartbeat`, and is a member of
`pg_signal_backend`; it has no creator-schema DML and no head write privilege.
The leader may terminate an old `active_pid` only after one query proves the slot
name equals the exact Datastore-derived name, `database = current_database()`,
`slot_type = 'logical'`, plugin `pgoutput`, and the backend role and
`application_name` equal the provisioned relay replication identity. Failure of
any predicate refuses termination. Secret refs are basenames in a required mount
directory, opened relative to a pre-opened directory descriptor with no symlink
following and owner-only permissions; rotation names a new file and files are
never rewritten in place. A missing, malformed or over-permissive secret makes
only that Datastore `Unavailable`.

**Desired, serving and observed epoch are distinct.** Topology supplies control's
desired epoch and its durable worker-visible serving epoch; the stream supplies
observed epoch through the marker. New registration requires all three to agree.
An existing feed may finish frames through the epoch where observed equals
serving even when desired has advanced; it stops at the marker and cannot deliver
an E+1 frame until all three equal E+1. So topology-first gives new registrations
`EpochPending` while existing connections finish epoch-N frames, and
marker-first pauses at the marker.

Matching desired and observed is not yet permission to confirm the marker. The
relay keeps `highest_gap_covered_lsn` below that marker's commit, posts the
fenced ACK, and waits until control durably promotes serving. It then atomically
appends one sequenced `Epoch` fence to every active grantee ring for that
Database, marks the local Database serving, unpauses delivery, and only then
admits or confirms the marker commit. A crash before the ACK commit replays the
marker; a crash after it may resume beyond the marker because control's serving
epoch is durable.

**Zero tokio.** `compio-postgres`, `compio::time::sleep`, `flume`, compio `ntex`
and `cyper`. No async runtime is added.

### Resume, confirmation and dedup

Each Datastore stream starts at its exact slot's `confirmed_flush_lsn` by asking
for `start_lsn = "0/0"`. That is not a worker resume coordinate; worker resume
uses `AppCursor`.

**The crash contract is state convergence with an explicit gap, not
at-least-once delivery.** The ring is volatile. Once a complete transaction's
frames are admitted to the right rings the relay may confirm its commit LSN
without a worker acknowledgment. If the process dies after that and before a
cursor reads them, the replacement cannot replay them from PostgreSQL and does
not pretend otherwise: it acquires a strictly higher term, and every older-term
registration receives `Registered::Reset(RelayFailover)` before any new-term app
data. Live queries refetch and converge; raw subscribers are told an interval was
lost. Duplicates may also occur around failover.

The driver's `advance_lsn` (`libs/compio-postgres/src/replication.rs:1495`) is a
durability promise that permits PostgreSQL to recycle WAL, and its API requires
acknowledgment or persistence first. Volatile ring admission satisfies neither.
The relay **replaces** that method, with no alias:

```rust
enum ConfirmationBasis { DurableHandoff, GapFenced { leader_term: u64 } }
ReplicationStream::confirm_lsn(lsn, basis)
```

`GapFenced` is used only after `leader_term` has committed in the coordination
database and the coverage rule holds. It states openly that PostgreSQL may
recycle WAL even though row frames are not durable, because loss of the volatile
state forces a higher-term reset. Any loss of ring or sequence state makes the
leader self-demote rather than continue in the same term. The durable fact is the
monotonic term that makes the gap observable.

Coupling confirmation to worker acknowledgment is rejected: one slow or dead
worker would pin a Datastore slot and grow cluster-shared WAL. A persistent spool
would give at-least-once delivery but adds a second durable row-data store with
its own retention, deletion, encryption, access-control, replication and write
amplification. If lossless raw CDC becomes a product requirement a durable spool
is required; acknowledgment coupling remains rejected either way.

**Confirmation is `min(wal_end, highest_gap_covered_lsn)`.**
`highest_gap_covered_lsn` advances only when every affected app is covered by one
of three states: every frame through that position is appended to its ring; the
decoder proved the interval contains no published frame for it; or an `AppReset`
was appended before the app entered an active reset covering the dropped
interval. It means "replayable in this term, explicitly reset in this term, or
reset on the next term", never durable delivery. Coverage advances only to a
decoded `Commit.end_lsn`; a keepalive is not decode evidence.

**Put the clamp on the operation, not one call site.** `advance_lsn` has three
call sites in one loop today (`crates/zeroship-plugin-db/src/wal_consumer.rs:330`
keepalive, `:343` `Commit.end_lsn`, `:384` mid-transaction `wal_end`) and the
first and third have the same defect: `wal_end` is the SERVER's end of WAL and
sits past every earlier transaction's commit record, whose frames may still be in
flight to the ring. Fixing only the keepalive arm produces a relay that confirms
unreplayable WAL just during long transactions - rarer, harder to reproduce,
identical in consequence. All three become one `confirm(pos)` helper that clamps
and calls `confirm_lsn(clamped, GapFenced { leader_term })`. At the commit arm
the clamp is a no-op by construction.

**Idle progress comes from a decoded heartbeat transaction.** Provisioning
creates `__zeroship_cdc.heartbeat(id boolean primary key check (id), nonce bigint
not null)`, owned by an operator no-login role, published as `(id, nonce)`. The
relay's least-privilege SQL role receives `UPDATE (nonce)` and no creator-schema
privilege. When `wal_end > highest_gap_covered_lsn` and no decoded commit has
advanced coverage for 10 seconds, the relay bumps the nonce and commits, then
advances coverage only when pgoutput delivers that transaction's decoded
`Commit.end_lsn`. One transaction and one ordinary SQL connection per idle
Datastore is the accepted cost. **Granting `pg_logical_emit_message` to the relay
is rejected** because that role could then forge epoch markers. Unrelated WAL in
another database may remain behind the slot for up to the heartbeat interval;
that bounded retention is intentional and replaces the false claim that a
keepalive proves an idle Datastore decoded through `wal_end`.

**The dedup key is `(commit_lsn, change_index)`, lexicographic.** `commit_lsn` is
available on the FIRST frame of the transaction (`Begin.final_lsn`,
`libs/compio-postgres/src/replication.rs:1949`, "LSN of the commit record"), so
the relay stamps it as it decodes without buffering. `change_index` is the
0-based pgoutput DML ordinal, incremented exactly once per decoded
Insert/Update/Delete **before** projection, Grant lookup or fan-out, and reset at
every `Begin`. Every fan-out copy of one decoded row carries the same ordinal and
a row with zero active Grants still advances the counter, so topology changes
cannot renumber a replay. It is per transaction, not global: a global counter
would not survive a relay restart, because resume re-delivers whole transactions
from their `Begin`.

The rule: the worker keeps the highest `(commit_lsn, change_index)` it has
applied per app and drops anything lexicographically at or below it. One
comparison of a `u64` and a `u32`. Not a set. Salesforce's three-header design
(`commitNumber` + `transactionKey` + `sequenceNumber`) solves transaction
reconstruction across a lossy replay-id bus; we have one ordered stream per slot
and never reassemble a transaction, so `transactionKey` buys nothing.

**Ordering.** pgoutput delivers changes in commit order within one slot, so
per-app order is preserved as long as the relay does not reorder. One ring per
app with a single writer is the constraint, and it is why the ring is per app
rather than global.

### Watermark invalidation

Two pieces of worker state are not conflated. A higher `leader_term` always
invalidates the per-term `AppCursor` and subscriber state and clears the numeric
watermark. A changed `systemid` or `timeline` invalidates the numeric watermark
too, because the same LSN may now name different WAL.

The complete rule is closed rather than inferred from the reason name.
`Initial` has no prior watermark. `RelayFailover`, `GrantRebound`,
`DatastoreReconnect` and `SystemIdentityChanged` **clear** it. `RingOverrun`,
`DatabaseEpochChanged`, `AppDegraded`, `AppReactivated`, `SlotInvalidated` and
`ClassificationChanged` **retain** it on the same
`(systemid, timeline, database_id, grant_generation)`. Every reason still
refetches live state. A reason absent from these two sets is a wire-decode error,
not a guessed default.

Each ring binding retains `latest_watermark_clear: Option<(seq, ResetReason)>`
after the corresponding `AppReset` is evicted, for the binding lifetime rather
than `ring_retention`. A cursor with `next_seq <= seq` receives that stronger
reason, never a watermark-retaining `RingOverrun`.

**Every same-term ordinary Datastore reconnect outside an active reset generation
emits `AppReset(DatastoreReconnect)` and clears the numeric watermark before
delivery**, because crash recovery comes up on an unchanged `(systemid,
timeline)` while a filesystem, EBS, ZFS or LVM snapshot restored without
`recovery.signal` rewinds the WAL. A higher term uses `RelayFailover`; a
reset-owned fresh connection uses that generation's immutable kind and emits no
second reconnect reset. This turns a network interruption into refetches and
possible duplicates, accepted because the relay has no trustworthy
server-incarnation signal.

On every leader acquisition and every Datastore replication connect the relay
reads `(systemid, timeline)` from `IDENTIFY_SYSTEM` and records them beside the
Datastore topology revision. Today `wal_consumer.rs` fetches that value and
discards it: the success arm of `conn.identify_system().await` has no binding.

- A `systemid` differing from the Cluster's is first treated as a **misrouted
  tenant DSN**: fail that Datastore closed, update no topology, drop no slot,
  serve no rows. A legitimate restore uses one operator-only
  `RotateCdcClusterIdentity(cluster_id, expected_old_pair, new_pair)`, which
  proves every configured DSN reports the new pair, compare-and-swaps both
  identity columns in one transaction, increments the topology revision and puts
  every Datastore into `Quiesce { SystemIdentityChanged }` at a new generation.
  Automatically trusting a changed identifier could connect a relay to another
  tenant cluster.
- With the system id unchanged, a larger timeline is a promotion or PITR: the
  leader proves every Datastore connection reports it, submits a fenced
  `AdvanceCdcClusterTimeline(expected, observed)`, and self-demotes. A lower
  timeline fails closed for operator recovery. Persisting the expected timeline
  is what lets a replacement process make this decision; process memory is not
  the authority.
- `SELECT timeline_id FROM pg_control_checkpoint()` is a diagnostic oracle for
  the watchdog surface only. `IDENTIFY_SYSTEM` on the streaming connection is
  authoritative, because it is the connection the stream runs on.

### Failure behaviour

**WAL is bounded by `max_slot_wal_keep_size`, launch value `8GB`.** Provisioning
sets it cluster-wide and reloads configuration; relay readiness refuses `-1` or a
live value different from 8192 MiB, and the cluster reserves at least 12 GiB of
free WAL filesystem headroom beyond normal peak before a Datastore becomes
CDC-ready. No production WAL-rate measurement exists, so 8 GiB is an explicit
provisional assumption. It is `context = sighup`, so setting it needs a reload,
not a restart.

**Slot invalidation has one canonical order: reset, then drop, then create.**

1. Publish `Quiesce { SlotInvalidated }`, acquire the exclusive Datastore fence,
   cancel and join the decoder.
2. Append `AppReset(SlotInvalidated)` to every affected ring, activate reset
   coverage, clear egress, close any response that cannot take the control.
3. Drop the exact lost slot, prove it absent, acknowledge `Quiesced`.
4. Submit and durably validate the complete-head snapshot, then publish
   `Restore`.
5. Create the fresh exact slot at the current WAL position, decode its heartbeat,
   seed observed from that snapshot, acknowledge `RestoreReady`, observe durable
   `Ready`, release the fence before worker data resumes.

If recovery also changes process and term, registration observes the stronger
`RelayFailover` instead; the protocol does not promise an evicted ring's slot
reason survives a term change.

**The fatal-error classifier must key on SQLSTATE.** `is_fatal`
(`crates/zeroship-plugin-db/src/wal_consumer.rs:624`) matches lowercased
substrings - `58p01`, `does not exist` conjoined with `replication slot` or
`publication`, and `invalid slot name`. Slot invalidation is SQLSTATE `55000`
with "can no longer get changes from replication slot", which **none of the three
arms match**, so today's supervisor treats it as transient and retries forever at
the 30-second cap. `55000` from `START_REPLICATION` must route to the lost-slot
path above. There is a collision to be careful about: `replication.rs` already
maps `55000` to `wal_level_not_logical` with a comment calling it "the canonical
SQLSTATE when wal_level != logical". It is the canonical SQLSTATE for
`object_not_in_prerequisite_state` in general. In situ that mapping sits on
`pg_create_logical_replication_slot` and is not wrong; the comment's general
claim is, and a relay reusing it on `START_REPLICATION` would tell an operator
with a full WAL disk to go set `wal_level`.

**Per-app fault isolation, five rules in binding order.** One process now decodes
and fans out for every tenant on the cluster, so a defect that used to be per
worker becomes per cluster.

- **The decode loop touches no per-app code.** Decoding produces
  `(schema, relation, tuple)` and nothing else; everything app-specific happens
  on that app's ring writer, downstream of the Datastore/Database/Grant lookup.
- **Every per-app step is total.** It returns `Result`, and its error becomes a
  `Gap` or an `AppReset` for that app. Checkable rather than aspirational:
  `clippy::unwrap_used`, `clippy::expect_used` and `clippy::indexing_slicing` at
  deny level on the relay crate, which `./tests/clippy_gate.sh` already runs
  under `--all-features`.
- **A panic that still happens kills the process, deliberately. Do not
  `catch_unwind` per-app work.** compio is one runtime; catching a panic inside a
  task leaves whatever it was mutating in an unknown state, and the state here is
  a slot cursor plus confirmation and dedup state. A relay that dies releases the
  advisory lock and a standby takes over. A relay that limps is a silent per-app
  data-loss machine that still holds the lock.
- **One app cannot starve another.** The ring writer is O(1) per frame and
  allocates nothing proportional to subscriber count.
- **A poisoned app is quarantined, and quarantine never touches the slot.** An
  app reaching `app_fault_threshold` projection/encoding failures inside
  `app_fault_window` appends `AppReset(AppDegraded)`, atomically marks the
  binding reset-active and invalidates every connection cursor, then stops
  writing its row frames at the tenant filter. While reset-active, dropped
  changes satisfy the third coverage arm, so the relay keeps decoding and
  confirming. Reactivation is attempted once after `quarantine_duration` by
  encoding the next app frame into a temporary buffer: success appends
  `AppReset(AppReactivated)` then the frame, failure emits nothing and restarts
  the timer. Connection-local `Initial`, `RingOverrun`, slow-consumer close and
  `RelayFailover` never count toward the threshold, so one hostile worker cannot
  quarantine an app for healthy workers.

**What the relay measures, and why the obvious signals are blind.** The relay
confirms the slot once frames are admitted to volatile rings, not once a worker
has them, so **`confirmed_flush_lsn` advances at FULL SPEED while delivery to
every worker is failing**. The most reached-for PostgreSQL health number moves
fastest exactly when the service is most broken, and `watchdog_query`'s
`lag_bytes` is computed from it and is blind the same way. `safe_wal_size` is
blind twice: it is NULL until `max_slot_wal_keep_size` is finite, and once
numeric it measures the slot, which is precisely what stays healthy through a
delivery outage.

The signals that are not blind are relay-side, per app or per Datastore:

| metric | why it is not blind |
| --- | --- |
| `cdc_delivery_outage_age_seconds{app_id}` | **The delivery health signal.** Max, over connected cursors owing a frame or reset, of time since that cursor last advanced. Preserved across ring eviction; cleared only on progress or close. Depends on no worker report |
| `cdc_ring_depth_bytes` / `cdc_ring_depth_frames{app_id}` | The buffer fills exactly when delivery fails |
| `cdc_resync_total` / `cdc_gap_total{app_id,reason}` | Loss counters, split by the closed reason enums |
| `cdc_connected_workers{app_id}` | "No subscribers" and "every subscriber wedged" both leave the ring shallow, because the relay evicts. Without this they alert identically |
| `cdc_published_columns{datastore_id,database_id,collection}` | A published set strictly inside the declared wire set with no migration in flight means a shrink whose widen never ran |
| `cdc_slot_wal_status` / `cdc_slot_safe_wal_bytes{cluster_id,datastore_id}` | Slot loss and WAL budget belong to one Datastore; an app label would hide shared capacity |
| `cdc_leader{cluster_id,relay_id,term}` / `cdc_topology_revision{cluster_id}` | The election pair and fan-out authority an operator checks before interpreting delivery metrics |

The exact label sets will drift once the relay is written; the durable content is
the paragraph explaining why `confirmed_flush_lsn` is blind. If the table and the
paragraph ever disagree, believe the paragraph. One PostgreSQL-side number is
honest: `restart_lsn` lag, pinned by the oldest transaction the server still
needs and unmovable by the client. It goes NULL on invalidation, so `wal_status`
stays primary and `restart_lsn` secondary. This surface is the moved
`watchdog_query` plus these counters on the relay's own HTTP server, not a
creator-visible namespace.

### Acceptance suite

The mask-only fixture is the security gate, and it spans the real producer
boundary on live PostgreSQL 18.4:

1. Apply a real migrate-server bundle declaring an ordinary `patients.note` and a
   **mask-only** `patients.ssn` (`t.string().mask(...)`, no encryption),
   exercising `ResolvedIrDocument -> WireProjection -> bracket`. The creator does
   not declare `id`: the confined ceiling forbids author primary keys and injects
   `id` plus six other system fields
   (`policies/confined-system-shape.inject.toml:96-102`).
2. Insert through the real plugin-db write pipeline with a distinctive plaintext
   sentinel and a distinct mask. Direct SQL is not the write-side acceptance
   path.
3. Through an admin-only assertion connection prove the physical row stores the
   mask in `ssn` and the exact sentinel in `__zs_raw__ssn`. This positive control
   catches a test that never placed plaintext in the raw column.
4. `pg_publication_tables.attnames` must equal the complete typed,
   policy-resolved projection including every visible injected system field,
   rather than a hand-written three-column list; separately assert `id`, `note`,
   `ssn` present and `__zs_raw__ssn` and the historical `ssn_masked` spelling
   absent.
5. Capture decoded pgoutput immediately after the driver decoder and before any
   relay or broker projection (memory-only, never logging row bytes). `Relation`
   names `ssn`; the tuple carries the mask; neither the raw name nor the sentinel
   occurs in the captured bytes.
6. Consume the real relay response through the worker decoder: one positive
   `Change` reaches the broker with `ssn` and `note` visible and `ssn` masked,
   and no wire frame contains the raw name or sentinel. This is the no-drop
   control.
7. **Mutation: omit the column list from publication rendering.** The catalog
   arm, the raw pgoutput-name arm and the plaintext-ingress arm must all fail. If
   only the catalog arm fails, the ingress hook is on the wrong side of the
   security boundary.

A second fixture covers the explicit `.mask({ kind: "none", classification:
"phi" })` shape: the declared physical column stores the sentinel, no raw sibling
exists, and the typed projection and `attnames` exclude the field entirely. A
builder-level negative arm tries to make it part of replica identity and must be
refused. Without it the ordinary mask-only fixture never exercises
`ExcludedProtected`.

The rest of the suite, each arm named by what it rules on:

1. **Replica-identity guard.** A projection omitting the replica identity is
   refused by the builder; separately, against a live server, such a publication
   makes `UPDATE` fail with the measured `42P10`.
2. **`REPLICA IDENTITY FULL` incompatibility.** Set FULL on a column-list table
   and assert the write fails, referencing `wal_consumer.rs:443` so the next
   reader who acts on that comment finds the counter-evidence.
3. **Two publications, one table.** The relay classifies "cannot use different
   column lists" as fatal and names the table.
4. **New column defaults out.** `ADD COLUMN`, insert, assert absent from the
   frames. Fails if someone ever "fixes" the publication to
   `FOR TABLES IN SCHEMA`.
5. **Wire golden and rejection corpus.** Exact hex goldens for one
   `SubscribeRequest` and all eleven frame tags covering every nested
   discriminant, both `Option` tags, a typed id, a full-width LSN and a nonempty
   `CellValue`; decode and re-encode byte-identically. Negatives: zero and
   max-plus-one lengths, unknown tags, Boolean `0x02`, invalid UTF-8, a
   noncanonical id, an out-of-domain generation, a false vector count,
   truncation, trailing bytes. Table-drive every closed HTTP problem code with
   its exact status, problem content type, absent `Hello`, and terminal or
   jitter-retry classification. Pin the permit commitment golden: mutating any
   non-permit request byte fails redemption, and hashing the permit into its own
   preimage must not match.
6. **Epoch marker.** A collection-changing migration produces the WAL `Message`,
   and the worker `Epoch` frame appends only after the desired/observed/serving
   rendezvous. Decode the same transaction twice on 18.4: with `messages = true`
   require the ordered `Message`; with the option omitted require all surrounding
   DML, no `Message` and no decoder error. Inspecting the option value is not a
   substitute. Assert both four-argument overloads are revoked from `PUBLIC`,
   that the migration service's provision connection can execute the text
   overload, and that app, worker and relay roles each get `42501`. The arm must
   name the overloads, so the PostgreSQL-16 signature cannot accidentally pass.
7. **Epoch commit rendezvous.** Run both races (marker before topology, topology
   before marker); in the topology-first arm a worker carrying serving E must get
   `EpochPending`, not `StaleExpectedBinding`, and comparing against desired E+1
   first must fail the test. Kill before the ACK commit and require marker
   replay; kill after durable ACK and require the new leader to initialize
   observed from equal desired/serving. Invalidate the slot while desired is E+1
   and serving E and require the complete-head handshake to adopt the exact
   pending record. Commit T4 at E+1, crash the migration process before its first
   POST, invalidate the slot in the same term, and require the reset snapshot to
   carry the exact rotation and hash.
8. **Service-assertion replay is fleet-wide.** Race one assertion through two
   verifier instances on the same replay table; exactly one atomic claim wins.
   Fail over mid-lifetime and require `401` on replay. Make the store unavailable
   and require `503 AuthenticationStoreUnavailable` before body decode.
   Substituting `InMemoryReplayStore` must fail both replica arms.
9. **Datastore cardinality and fixed publication.** Each exact slot decodes only
   its own database; draining A's slot from B is refused; the same slot name
   cannot be created twice cluster-wide. Adding a Database to A delivers without
   restarting A's stream; adding Datastore C requires a new stream. With stock
   settings reserve ten slots and assert the eleventh Datastore fails before
   becoming CDC-ready.
10. **Slot invalidation.** Same process, relay id, term, rings and responses;
    churn under a small `max_slot_wal_keep_size` until `lost`. Assert SQLSTATE
    `55000` and the full canonical order. A process-restart control observes
    `Reset(RelayFailover)`, not the slot reason. **Moving drop before reset must
    fail.**
11. **Leader election and coordination scope.** Two instances for one cluster,
    two Datastore streams: exactly one holds the lock, both streams belong to it,
    a clean kill releases it, and the survivor's term is strictly greater. In the
    half-open arm, blackhole the coordination TCP path and require the bounded
    probe to trip and cancel every stream and response. A negative fixture taking
    the same numeric advisory key in two Datastores proves both locks succeed and
    those connections cannot be the election authority. `zeroship_cdc_coord`
    updates only the leader pair; writes to fence, revision, identity or member
    state get `42501`. Two cluster ids with one system identifier, and one
    cluster whose two DSNs disagree, must both be rejected. On dedicated 18.4
    fixtures, promote a basebackup and require `IDENTIFY_SYSTEM` to preserve
    `systemid` and increase the timeline; crash-recover the same data directory
    and require both values preserved **and** an emitted
    `AppReset(DatastoreReconnect)`.
12. **Worker term and Grant fences.** Race permit issue, redemption, release,
    transition creation, fence completion and term increment; every path locks
    cluster before member/fence. Stale epoch or changed body changes no state;
    exact replay does not increment twice. Only exact death, credential
    revocation and a bound `SupervisorFence` unblock a second guarded worker.
    Mutation-remove the definer trigger, the cluster-first lock order, the
    pending guard, the local fence, or the receipt/death proof and require the
    corresponding arm to fail.
13. **Registration sharding.** 1,025 apps in one snapshot produce exactly two
    requests (1,024 and 1) sharing one generation with `shard_count = 2` and no
    app in both. An identical-body retry after a dropped `Registered` supersedes
    rather than returning `NonMonotonicRegistrationGeneration`. One byte changed
    under an admitted key returns `ConflictingShard`, cancels both candidates and
    marks switched apps reconnecting; retrying the poisoned generation returns
    the same code with no replacement response. Shard zero must deliver without
    buffering behind a held shard one.
14. **Topology acknowledgment and Grant fan-out.** Two Grants on one Database
    receive one row with the same `(commit_lsn, change_index)`. Hold old-binding
    rows in relay egress, worker decode and subscriber delivery, then
    revoke/rebind: nothing may be exposed until both evidence sets arrive. Hold
    R1's barrier on failed Datastore A and publish R2 for healthy B; a cumulative
    R2 ACK completes B while A stays pending. **Mutation: replace the
    per-Datastore greatest-revision rows with one cluster-wide pending latch, or
    let a prior-pair ACK complete a barrier, and require failure.**
15. **Crash after confirmation, before delivery.** Pause a cursor, commit a
    sentinel row, stop the leader at a failpoint reached only after ring
    admission and after `confirmed_flush_lsn >= commit_end_lsn` but before the
    cursor reads it. `SIGKILL`, higher term, reconnect with the old cursor:
    `Registered` reports `Reset(RelayFailover)` and the local `Resync` precedes
    any new-term `Change`. Arrange the old `next_seq` to equal the new empty
    ring's `ring_next_seq`, so bounds alone would accept it. **Mutation: delete
    both stale-session checks (`relay_id` and `leader_term`), not only one.**
16. **Registration and relation priming.** Resume inside a retained interval
    whose `Relation` is older than the cursor. Table-drive all seven
    `RegistrationRejectCode` values with their exact retry policies. Rebind A to
    B at equal epochs and require `GrantRebound`; swap cursors between two apps
    and between two worker ids and require `CursorBindingMismatch`. Combine a
    higher term with a rebind to a numerically lower LSN and require
    `GrantRebound`, not `RelayFailover`, plus a cleared watermark. Evict a
    `DatastoreReconnect` reset and reconnect from before its sequence:
    `latest_watermark_clear` must preserve the stronger reason. Table-drive every
    `ResetReason` through both the shared and the connection-local path and
    assert one closed watermark decision.
17. **Dedup under write concurrency.** A opens and holds, B inserts and commits,
    A commits; both rows must reach the subscriber. **This is the arm a
    per-change-LSN watermark fails and `(commit_lsn, change_index)` passes**, and
    the only arm a single-writer test cannot substitute for. Pair with the
    sequential control, which passes under either rule.
18. **Dedup under `heap_multi_insert`.** `COPY` three rows, assert three events.
    Control: the same three rows as one multi-`VALUES` `INSERT`, which produces
    three distinct LSNs and passes even under the broken rule, so the two arms
    together locate the defect rather than merely detecting it.
19. **The confirmation clamp.** With the ring stalled and WAL generated in
    another Datastore, a `PrimaryKeepalive` beyond this stream's gap-covered
    position must not move the standby status update. Idle until the heartbeat
    commits and require coverage to advance exactly to its decoded
    `Commit.end_lsn`. On an ordinary admitted commit the clamp is a no-op; if
    that arm goes red the admission ordering broke elsewhere. Assert every update
    used `GapFenced` and mutation-delete the explicit basis.
20. **The publication bracket.** A migration dropping the ordinary published
    `body` field succeeds (without the bracket it aborts `2BP01`), publishes the
    intersection between shrink and widen, and puts the marker in the widen.
    **Mutation: move the reconcile back to after the DDL and require failure** -
    the only way to prove the bracket is what fixed it. Add a non-prefix journal
    arm where A is skipped and later B applies. Crash after DDL before T4 and
    retry with the same exact-checksum bundle. Crash after T4 with control still
    serving E and require the head-ahead recovery branch with no shrink or DDL.
    Reapply a fully effective bundle and require an empty `PlannedDelta`;
    replaying its `createTable` into the folded checkpoint must fail the test.
    Submit a squash over an effective prefix and require both deltas to carry
    `RecordSupersessionNoOp` with `up` never running and `folded_schema`,
    publication, roles and epoch byte-identical; the fresh-Database control must
    classify it `ApplyOps`. Run an all-skip apply and require the no-rotation
    repair with no epoch and no marker. On a live decoder, hold the
    `DROP TABLE` + `ADD TABLE` swap uncommitted, commit, and write before and
    after: both row transactions arrive with no observable relation-absent
    interval.
21. **Two Databases, one Datastore publication.** Reconcile A while B's tables
    are members; B's `attnames` must be byte-identical before and after.
    **Mutation: restore `SET TABLE` and assert B's tables disappear.** This is
    the arm that would have caught the shared-object defect.
22. **Classification reset barrier.** With a plaintext `ssn` change retained
    behind the slot, apply a migration adding its mask and capture decoded
    ingress across both decoder instances. The old decoder must be cancelled, its
    socket closed and its task joined **before** any reset is appended, ring
    cleared, ACK sent, slot dropped or shrink begun; no raw name or plaintext
    sentinel may occur anywhere in the process-wide capture. **Mutation: leave
    the old decoder live, or keep the old slot, and require the ordering or
    plaintext-ingress arm to fail.** Repeat with a bundle that renames plaintext
    `legacy_ssn` to `ssn`, renames its table, then classifies it: the renderer
    must follow one `ProjectionFieldKey` through both operations and reset before
    DDL, and comparing only qualified names must fail this arm. In another
    bundle, drop plaintext `ssn` and create a protected `ssn` under the mapped
    table name; the historical-name collision must trigger the same reset.
    Mutation-delete either lineage rule.
23. **Slow consumer cannot reach the slot.** One connection that stops reading, a
    second app writing normally: the second app is unaffected,
    `confirmed_flush_lsn` keeps advancing, the stalled app's
    `cdc_delivery_outage_age_seconds` grows, and the stalled connection closes
    after `slow_consumer_timeout`. **The floor is the number of frames the
    healthy app delivered during the stall**: an arm delivering zero frames
    passes every other assertion trivially. Separately consume every egress
    permit, require `503` before queue allocation, close one response and prove
    exactly one permit frees. **Mutation: allocate before acquiring the
    semaphore.**
24. **Quarantine covers confirmation.** Five projection/encoding failures inside
    the window enter quarantine on the fifth and not before; ring overruns, stale
    cursors and slow-response closures increment nothing. On entry the reset
    precedes the first dropped row, every cursor resets or closes,
    `highest_gap_covered_lsn` advances through later dropped rows, and a healthy
    app keeps receiving. **Mutation: drop rows without the active-reset coverage
    arm and require the confirmation invariant to fail.**
25. **Gap and Truncate.** An over-budget value emits `Gap` for that row only;
    `TRUNCATE` emits `Truncate`; unchanged TOAST is `Unavailable`, not `Null`. An
    ordinary UPDATE decodes without an old tuple and an identity-changing one
    carries only the old identity as `pk`, deriving the new identity from
    `values`. Neither wire nor `ChangeEvent` has an old tuple. Repeat through
    SQLite preupdate.
26. **Gate arms.** Per `AGENTS.md`, every arm declares the number of items it
    ruled on and a floor. The floor that matters is the number of published
    columns the projection test inspected: an arm inspecting zero columns prints
    exactly what a clean tree prints.

### How to measure the added latency

No figure appears in this document because none exists.

Four timestamps per event: `t0` the commit timestamp pgoutput already carries
(the **server's** clock, so any figure mixing it with a relay clock includes skew
and must say so); `t1` the relay receiving `XLogData`; `t2` the relay writing
into the app's ring; `t2b` a connection cursor reading it out; `t3`
`broker::publish` returning in the worker.

**Report `t3 - t1` as the service's own contribution**, which is skew-free, and
report `t1 - t0` separately and labelled as containing skew. A single `t3 - t0`
buries the one quantity the service controls inside one it does not.
**`t2b - t2` is queueing delay and must be reported separately from `t2 - t1`**:
decode cost scales with row width and transaction size, queueing scales with
subscriber health, and one relay-side number folds a slow consumer into what
looks like a slow decoder.

Baseline: the same measurement against today's in-worker consumer, same `t0` and
`t1`, `t3` in the same process. Without that arm the number is unanchored - the
question is not "how long does the relay take" but "how much longer than today".

Load shapes, at minimum: one app one subscriber (floor); one app with a burst
larger than the ring (the `Resync` boundary); N apps writing concurrently where
only one has subscribers (the case the per-app-slot design was bad at); and one
app with a deliberately stalled subscriber alongside N healthy ones, reporting
the healthy apps' distribution - the only run that shows whether the
never-block invariant holds under load rather than in a unit test.

Report p50, p99, p99.9 and max, not a mean: a relay that stalls 200 ms on the
librdkafka pattern has a fine mean.

Two derived inputs, not results: **worker restart time** wall clock from process
exit to first frame consumed after reconnect (`ring_retention` must exceed it
comfortably or an ordinary deploy becomes a fleet-wide `Resync`), and **per-app
frame rate and mean frame size** under real write load (`ring_bytes_per_app`
divided by their product is the disconnect tolerance the ring actually buys).

Invalidated by: a run where relay, workers and database share a machine, since
io_uring completion queues and the WAL writer contend; and a run with no
subscriber, since `has_subscribers` short-circuits before the expensive work and
would measure the short-circuit.

---

## Why it is this way

**PostgreSQL is the security boundary, not a downstream filter.** The publication
column list is a whitelist over declared fields, so everything not on it is
absent by default *including columns that do not exist yet*. A downstream
raw-column stripper is a name-based blacklist and protects only names it
anticipates. The relay cannot leak a value it never received. This is a different
class of guarantee from "one filter at one call site", which is what the
flip-write-path review could achieve and honestly labelled *guarded* rather than
unrepresentable.

**The whitelist does not repair a current creator-response leak.** The shipped
serializers are safe: `message_to_json` and `ws_frame` filter platform names
through `creator_visible_columns` and emit no row values
(`crates/zeroship-data-core/src/broker.rs:1020`, `:1093`). What the whitelist
does is move the boundary one process earlier, so the raw name and value never
reach pgoutput, the relay, or the broker. The accepted cost is that every schema
migration must maintain that whitelist correctly.

**Measured PostgreSQL behaviour that binds the design.** Every claim below is
version-sensitive in the literal sense that a later major may change it, and the
platform target is PostgreSQL major 18 with 18.4 as the verification and
deployment release. That is a decision made here, not an inference from the
migration-feature matrix, and the platform is not aligned with it yet.

| behaviour | consequence | evidence status |
| --- | --- | --- |
| A column list removes the excluded NAME as well as the value; a new column defaults OUT | One mechanism covers both `new_tuple` and `changed_columns`, which is built from the `Relation` message | filtering repeated on 18.4; the **new-column arm has only 16.15 evidence** and the 18.4 fixture must establish it |
| A list not covering the replica identity is accepted as DDL and then fails every UPDATE and DELETE with `42P10` | `replica_identity_columns` is a UNION term, not an assumption. A column-list mistake is a WRITE OUTAGE, not a read failure | 18.4, target evidence |
| `REPLICA IDENTITY FULL` is incompatible with any partial column list | The tree's own comment (`wal_consumer.rs:443`) recommends FULL as the fix for missed deletes. **The two improvements are mutually exclusive**; this design chooses the column list | 18.4, target evidence |
| Two publications with different lists on one table break decode, at decode time | The relay treats that decode error as fatal for the Datastore and names the table rather than reconnecting into it | 18.4, target evidence |
| A published column is a catalog dependency: `DROP COLUMN` is `2BP01`, and `CASCADE` removes the WHOLE TABLE from the publication with only a `NOTICE` | Forces shrink-before-DDL-widen-after. CASCADE is worse than the error | 18.4, target evidence |
| Publications are Datastore-scoped and one logical slot decodes one Datastore | One slot, one stream, one shared publication per Datastore; slot namespace and budget are cluster-scoped | 18.4, target evidence |
| A publication member's column list changes atomically via `DROP TABLE` + `ADD TABLE` in one transaction, and a mid-stream `ADD TABLE` is picked up live without disturbing existing streams | The bracket does not interrupt sibling Databases | 18.4 for the catalog mechanics; **the concurrent-decoder atomicity observation is 17.11 only** |
| Per-change LSNs go BACKWARDS across commit order (transactions are delivered in commit order but a change's LSN is insertion order) and COLLAPSE under `heap_multi_insert` | The dedup key is `(commit_lsn, change_index)`. "Keep the highest per-change LSN" silently discards rows on two ordinary concurrent writers | 18.4, target evidence |
| Transactional logical messages are ordered in WAL with the DDL and later changes, and are omitted entirely unless `messages=true` | The epoch marker rides the widen transaction; the relay sets the option explicitly with no fallback | ordering has 18.4 evidence; **the negative survives only on 16.15 and 17.11** |
| `max_slot_wal_keep_size` boot value is `-1` (unbounded) and is `sighup`; on exhaustion the slot goes `wal_status = lost`, `restart_lsn` goes NULL, and reads fail `55000` | The GUC must be set, and `lag_bytes` (computed from `restart_lsn`) goes blind, so `wal_status` is primary | slot/walsender/`logical_decoding_work_mem` defaults repeated on 18.4; **the invalidation transcript did not record its server and must be repeated** |
| Promotion changes timeline while crash recovery does not | Crash recovery comes up on an unchanged pair after a rewind, so an unchanged pair cannot be trusted; every same-term reconnect resets | **17.11 only**; the 18.4 failover gate must repeat it |
| `safe_wal_size` becomes numeric only with a finite WAL cap | It is never a portable default, and it measures the slot, not delivery | 18.4, target evidence |

**One version trap admits two simultaneously true readings.** PostgreSQL 16
exposes three-argument `pg_logical_emit_message` identities; 17 and 18 expose
four-argument identities with a defaulted `flush`. A three-argument call can look
portable while an ACL statement naming either 16 identity matches nothing on 18.
This design chooses the 18 identities explicitly. It is exactly the split that
passes in a 16-based test and fails in an 18 deployment.

**Privilege follows the process.** The relay is a separate service that runs no
creator code, which is what lets it hold `REPLICATION`. Giving the worker a
second `REPLICATION`-only login used by a dedicated thread is NOT a boundary
under the `AGENTS.md` invariant, and this design argues against it. It is the
cheap option and someone will propose it.

**A binary that nothing links has no shared-vocabulary problem, and that is what
dissolves a question rather than answering it.**
`docs/proposals/2026-08-31-data-crate-shape.md` carried an open item asking how
the relay would share `pg_error::classify` with `zeroship-data-postgres`, and
offered three options: the relay depends on `data-postgres` (honest, but drags
the worker's data plane into the relay's closure); extract the classifier lower
(pushes a vendor translator toward `data-core`, which
`tests/data_crate_closure_gate.sh` refuses outright); or the relay carries its
own. All three answer "who must agree with whom about an error". Under the binary
shape there is nobody to agree with: no crate links this one, so a classifier
here answers only to this process. The third option is now correct by
construction rather than by preference, and the first two are answering a
question that no longer exists.

**Do not record that as "the classifier went with the relay" - the arithmetic
does not support it.** Measured across `crates/zeroship-plugin-db/src`: 13
`pg_error::classify` sites, nine of them in `replication.rs`. Mapped to their
enclosing functions, those nine split 3 / 1 / 5 - three in `ensure_worker_slot`
(relay-only), one in `watchdog_query` (live V8 caller, stays), five in the drop
family (called from both sides today, by `change_stream_pg.rs` and by
`service.rs`'s `deprovision_app`). The adapter keeps `classify` either way, at no
cost, because `zeroship-data-postgres` is already in the worker's closure. What
changes is only that the relay writes its own for its three.

**A consumer must never be able to reach the slot.** Everything in the
slow-consumer, ring-sizing and quarantine rules is a consequence. Vitess issue
11169 is the scar: a slow VStream client plus a capacity-1 buffer blocked
`servePrimary()` and hung a replica promotion. Consumer backpressure reached the
HA control plane. Ours would reach WAL retention, which is worse, because WAL
retention is shared across every tenant on the cluster and a replica promotion is
not.

**One process now decodes for every tenant on the cluster.** Supabase Realtime
had three project-wide outages from an un-trapped per-subscriber loop, three
unrelated root causes, same shape, smaller radius than ours would be. That is why
the decode loop touches no per-app code, every per-app step is total, and a panic
kills the process rather than poisoning a tenant.

**Accepted costs, taken deliberately. None of these is a defect list, and none
should be "fixed" without reopening the decision it belongs to.**

- **The relay is a single point of failure and failover loses an event
  interval.** One wedged process stops every live query on the cluster, and the
  leader lock makes a second instance a standby, not a second consumer. Failover
  time is bounded below by how fast PostgreSQL notices the dead session, which is
  a TCP-keepalive-shaped quantity and **not measured**. Worker fencing adds
  polling, permits, durable membership and death records, and one unreachable
  worker can block a term or Grant transition indefinitely. This availability
  loss is the strongest objection to the design. A bounded userspace lease is
  rejected: after its last clock check a descheduled process can resume inside
  delivery.
- **It is the new bottleneck.** One decode per Datastore is the PostgreSQL floor,
  but fan-out is `O(apps x subscribing workers)` in one process. A relay
  saturating one core on fan-out has no second core to move to without breaking
  per-app ordering. The ring-per-app structure admits sharding by app across
  threads later; nothing here does it, and doing it reopens ordering.
- **The migration service becomes load-bearing for creator WRITES.** It currently
  cannot break a creator's writes by getting a publication wrong; after this it
  can. That is a real transfer of blast radius from a read path to a write path,
  and the single strongest argument against the column list. The alternative -
  keep the publication wide and filter in the relay from a fetched descriptor -
  trades an availability risk for a confidentiality risk plus a staleness window
  on every deploy plus a new relay-to-deploy coupling.
- **`REPLICA IDENTITY FULL` is permanently unavailable**, and its replacement
  (the bounded delivered-pk set) is specified, not prototyped. If it needs
  unbounded state, the design traded a working fix for an idea.
- **The wire is a fourth serialisation boundary** after pgoutput, the broker's
  JSON and the SDK's TypeScript types.
- **The dedup key rests on pgoutput replaying a transaction's DML in the same
  order every time.** A strictly safer alternative exists and is not taken:
  transaction-granularity dedup, dropping every frame whose `commit_lsn` is at or
  below the watermark and advancing only at a transaction-boundary frame. It
  needs no index and no determinism assumption; it costs a boundary frame and
  redelivers a whole transaction on mid-transaction death. The index is chosen
  because it is exact; the argument for the boundary frame is that it is exact
  *without a premise*.
- **`heap_multi_insert` IS reachable by creators today.** `env.db.insertMany`
  emits multi-`VALUES` and does not collapse, but the migration guard permits
  two paths that do: plain `COPY ... FROM STDIN` is explicitly allowed (only
  `COPY ... PROGRAM` and `COPY` naming a file are denied,
  `crates/zeroship-migrate-postgres/src/guard/sql.rs:1401`, `:1404`), and
  `CREATE TABLE AS` is gated by target ownership rather than denied (`:923`). So
  `change_index` is load-bearing, not defensive.
- **The bracket makes a migration's publication work three times bigger**, adds
  an operator-visible shrunk-but-not-widened state, and serializes
  publication transactions for Databases sharing a Datastore.
- **Ten interacting knobs and a lint.** "No knobs" deploys correctly by
  construction; a ten-knob surface has a wrong setting available for every one.
  An operator setting `ring_bytes_per_app` too low converts normal traffic into a
  `Resync` storm, which becomes database read load charged to the tenant. The
  mitigation is the latency measurement, and a derivation nobody runs is a
  default nobody chose.
- **Three more frames the raw subscriber can ignore.** `db.subscribe` remains a
  non-lossless API with a creator-visible reset obligation
  (`sdks/db/src/subscribe.ts:50` documents `resync` as "client must re-fetch"; a
  creator switching on `ev.kind === "change"` silently diverges). `Gap`,
  `Truncate` and the three-way cell widen that hole to three ways to diverge.
  Live queries absorb all of them for free, because `rerun()` fires on every
  change event anyway (`sdks/db/src/live.ts:337-338`). This proposal does not
  make the reset unignorable, and the API must not be sold as an audit log.
- **The deny-level lints are the weakest of the over-specifications.** A lint has
  an `#[allow]` escape and the first awkward indexing site will get one. The
  property that actually holds is the one below it: a panic kills the process
  rather than poisoning a tenant.

**What would make a different choice right.** If `zeroship-stream` grew a topic
argument on `poll`, a batch publish and TLS - three changes to one crate, all of
which its own comments already contemplate - the case for a bespoke push channel
weakens considerably and the rejected durable-spool alternative becomes cheap. If
Datastore count approaches the ten-slot ceiling, the control plane must place new
Datastores on another physical cluster or schedule a restart with higher
`max_replication_slots` and `max_wal_senders`; combining decodes into one slot is
not an option, because PostgreSQL binds a logical slot to one Datastore.

---

## Settled

These are decisions, not questions. They are listed apart from Open so nobody
reopens one by reading a stale option list.

- **The service is `zeroship-data-cdc-server`, and it is a BINARY nothing links.**
  Operator ruling, 2026-09-03. The enforceable form of "nothing links it" and the
  one named exception are in "What it is"; `tests/data_crate_closure_gate.sh`
  arm 3 checks it mechanically, and both directions were mutation-proved when the
  arm landed.
- **Contracts live outside the relay, in two homes, and are not re-exported from
  it.** Wire contracts in `zeroship-cdc-wire`; data-plane contracts in
  `zeroship-data-core`. The relay may depend on `data-core` and today does not.
- **The relay's error handling is internal; it shares no classifier.** This
  replaces the three-option question the crate-shape proposal carried. See "Why
  it is this way" for the reason and for the 3 / 1 / 5 arithmetic that refutes
  the tempting summary.
- **Zero files move into the relay.** `wal_consumer.rs` is split and rewritten;
  `replication.rs` splits with its watchdog and drop halves staying;
  `slot_reaper.rs` is deleted whole in the privilege commit; `change_stream_pg.rs`
  stays in `zeroship-plugin-db`. A verbatim move of `wal_consumer.rs` compiles,
  passes every gate, and silently splits the process-wide broker.
- **The transport pair is proven** (`f7e043252`), and the frame layout was never
  at stake in it: a length-prefixed frame over a byte stream is
  carrier-independent. What remains of the original transport question is TLS and
  duration, carried as Open 4.

---

## Open

1. **NEEDS-DECISION: when do the Datastore/Database/Grant entities land?**
   Nothing in this document can be implemented until they exist. **This item said
   `DatastoreId`, `ClusterId`, `DatabaseEpoch` and `GrantGeneration` "have zero
   occurrences in `crates/`" until 2026-09-03; they now occur only as WIRE TYPES
   in `crates/zeroship-cdc-wire`**, which changes nothing about the blocker - no
   table, column or control-plane record mints one, so there is still no entity to
   key on. The design is settled and the decision needed is scheduling: the
   relay cannot be estimated, let alone started, until the decoupling work has a
   landing date. The Database rekey of the migration apply lock is inside that
   dependency, and shipping the bracket on the current app/project key would let
   two apps sharing a Database interleave it. **Creating
   `crates/zeroship-data-cdc-server` did not unblock this and was not intended
   to.**
2. **NEEDS-DECISION: when does the platform move to PostgreSQL 18.4?** Relay startup
   and Datastore provisioning read `server_version_num` and refuse a major
   outside `[180000, 190000)`; there is no 16/17 branch, signature probe or
   compatibility fallback. `deploy/compose/docker-compose.yml:73` pins
   `postgres:16` and CI exercises 16 and 17. Operators must upgrade and restart
   PostgreSQL before CDC can be enabled. Who schedules that, and for which
   environments, is not decided here.
3. **BUILDABLE (4h): repeat the three non-target measurements on 18.4.** The
   new-column-defaults-out arm (16.15 only), the `messages` negative (16.15 and
   17.11 only), and the slot-invalidation transcript (server version unrecorded).
   Until then those three claims are not target evidence.
4. **MOSTLY ANSWERED, 2026-09-03. What is left is TLS and duration.** `ntex` v3
   response streaming and `cyper`'s `stream` feature do compose into an
   incremental framed channel, and ntex's write backpressure reaches the body
   stream, which is what lets the ring bound its egress. The evidence is
   `crates/zeroship-cdc-transport-spike/` and the numbers are in the Status
   block. Two pieces of the original question are still open and neither gates
   the wire crate: the spike is plaintext, so TLS terminating in the relay
   process is unproven and needs an ntex feature the workspace does not enable;
   and the longest hold measured is seconds, not the multi-minute channel this
   item asked for. **This item claimed no code had been written against either
   library. That was false when written** - both ship today, in the gateway and
   in `fetch`/`compio-s3` respectively; see the Status block for the sites.
5. **BUILDABLE (8h): measure advisory-lock release latency after a leader loss.**
   Three kill shapes are not equivalent and only one is the real case; see the
   DO-NOT note in History. Governed by `tcp_keepalives_idle`, `_interval` and
   `_count` on the server, which is what the measurement should vary. It bounds
   the failover interval this design accepts.
6. **NEEDS-DECISION: is the bounded delivered-pk set prototyped, or is the delete
   contract changed instead?** It replaces `REPLICA IDENTITY FULL` for "row left
   the view" and is the one place a subscriber-side contract changes shape rather
   than moving. If it turns out to need unbounded state, the design must revisit
   either the column list or the delete semantics, and that choice is not made
   here.
7. **NEEDS-DECISION: who corrects `docs/architecture/data-system.md`, and does it
   block implementation?** It still states one publication per Database and
   Database creation adding a publication to a shared slot, which the corrected
   decode scope cannot support. It also states publication membership excludes
   the whole `__zeroship_` namespace, which cannot coexist with a decoded
   heartbeat. Two live documents currently disagree about publication
   cardinality.
8. **NEEDS-DECISION: do the ten ring knobs ship on provisional defaults, or does
   the latency measurement gate the relay?** Nothing here derives them from
   workload. The four inputs that would are p99 frame size, per-app burst bytes,
   worker-restart duration and concurrent subscriber count. Until that decision,
   implementations use the stated values rather than inventing their own.
9. **BUILDABLE (2h): profile the relay under a spilling transaction.**
   `logical_decoding_work_mem` boots at 64 MiB per slot, and a spilling
   transaction has never been driven through a ring. Memory sizing rests on it.
10. **NEEDS-DECISION: does the raw `db.subscribe` reset obligation stay
    creator-visible?** This design leaves it visible and ignorable, and adds two
    more frames plus a three-way cell that widen the same hole. Making the reset
    unignorable is a separate decision, and it is now more overdue than when
    there was one way to diverge instead of three.
11. **NEEDS-DECISION: `Change.pk` is undecodable IN MEANING under wire v1.** The
    encoding is committed and the semantics are not. This document defines `pk`
    as the replica-identity vector, but `Relation` never says WHICH columns those
    are: it carries the collection and the column list, and nothing marks the
    identity subset. A decoder can therefore read the bytes and cannot say what
    they identify without out-of-band schema knowledge. Either `Relation` gains
    an identity marker, or `pk` becomes positionally defined against something the
    frame carries, or the field's meaning is documented as
    resolved-by-the-descriptor and the consequence for a raw subscriber is stated.
    This is a v1 decision, not a v2 one: the frames shipped at `05d9462c8` and
    have no consumer yet, which is exactly when the shape is cheapest to change.
12. **NEEDS-DECISION: two wire id prefixes are unsettled, and one contradicts its
    own source.** `zeroship-cdc-wire` mints `ds_` for `DatastoreId`, but
    `docs/proposals/2026-08-28-app-database-decoupling.md:25` writes `ds_` in its
    entity block while `:46` argues for `dbs` - the same document, two answers.
    `clu_` for `ClusterId` is an invention pinned by neither document. **One
    measured fact for that discussion, because the `dbs` argument rests on it and
    it is false:** `:46` justifies three-letter uniformity by claiming
    `crates/zeroship-core/src/typed_id.rs`'s "every prefix is three lowercase
    letters". It is not - of the 27 prefix constants that file declares, 26 are
    three letters and one is four, `WORKFLOW_CRON_PREFIX = "cron"` at
    `crates/zeroship-core/src/typed_id.rs:504`. The convention is strong and it is
    not a rule, so a length argument cannot decide this on its own.
13. **NEEDS-DECISION: what happens to `crates/zeroship-cdc-transport-spike`?** It
    is a permanent workspace member with "spike" in its name - one of the 45
    packages `cargo metadata --no-deps` reports - and it carries the dev
    dependency that took `PINNED_ENTRYPOINTS` in `tests/zero_tokio_gate.sh` from
    nine names to ten. Three shapes are available and none has been chosen: keep
    it as a named transport conformance suite (rename, and it stops reading as
    scaffolding); fold its twelve tests into the relay crate when the listener
    lands (and lose them until then); or delete it and keep the numbers in this
    document (and lose the ability to re-run them). Deciding by not deciding
    means the name ships.
14. **NEEDS-DECISION, and it is a PRIVILEGE decision: when does the worker stop
    holding `REPLICATION`?** Extracting the crate left this exactly where it was;
    see "The reaper is a privilege change". The four edits must land in one
    commit, and three of four leaves either a worker holding an unused
    `REPLICATION` grant - the failure mode the `AGENTS.md` invariant names - or a
    booting worker refusing a posture it now correctly lacks. The prerequisite is
    Open 1: the relay cannot take slot ownership while slot provisioning has no
    `DatastoreId` to key on.

---

## History

Deliberation, prior-art survey and the decision to build a relay at all live in
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`. The prerequisite
entity work is `docs/proposals/2026-08-28-app-database-decoupling.md`. The
guarded-versus-unrepresentable vocabulary comes from
`docs/reviews/2026-08-28-flip-write-path.md`.

**Which document governs, so the next reader does not pick by which one they
opened first.** `docs/proposals/2026-08-31-data-crate-shape.md` also assigns the
four CDC modules, from the crate-split's point of view. **On those four files
THIS document governs** - it is the more specific one and has been hardened
against them twice - and the split proposal's placement table now says so and
carries the corrected row. Where they still disagree, the disagreement is a
defect in that table, not a choice.

DO-NOT notes, each recording a mistake that would otherwise be remade:

- **Do not use `ALTER PUBLICATION ... SET TABLE` on a shared publication.** It is
  a full replace computed from one app's schema, so two tenants migrating
  concurrently each silently remove the other's tables and the removed tenant's
  live queries just stop updating. The existing advisory lock does not fix this,
  and it looks like it does: it serialises the replace, so last writer wins
  cleanly instead of racily.
- **Do not narrow a column list with `DROP COLUMN ... CASCADE`.** CASCADE removes
  the whole table from the publication with a `NOTICE`, which is worse than the
  `2BP01` error it silences.
- **Do not reconcile the publication only after the DDL.** That order aborts on
  any migration dropping an ordinary published column.
- **Do not emit the epoch marker in the shrink transaction.** Shrink produces a
  state no descriptor describes, so the marker would announce an epoch the
  following frames do not match.
- **Do not key the dedup watermark on the per-change LSN.** Measured: it goes
  backwards between overlapping transactions and collapses under
  `heap_multi_insert`, and the failure is a permanently discarded row with no
  error anywhere.
- **Do not confirm a keepalive `wal_end`, and do not fix only that call site.**
  The mid-transaction `wal_end` arm has the same defect for the same reason;
  fixing one produces a relay that confirms unreplayable WAL only during long
  transactions.
- **Do not use a keepalive as idle progress evidence.** It is not decode
  evidence. The decoded heartbeat transaction replaces it.
- **Do not grant `pg_logical_emit_message` to the relay** to solve idle progress
  or anything else - that role could then forge epoch markers.
- **Do not hash `cluster_id` into an advisory lock key.** A collision makes two
  clusters exclude each other.
- **Do not use `InMemoryReplayStore` for the relay's assertion verifier.** The
  core module limits it to a single-replica callee, and leader failover makes the
  relay replicated.
- **Do not `catch_unwind` per-app work.** Catching a panic inside a compio task
  leaves a slot cursor plus confirmation and dedup state in an unknown condition.
- **Do not add a drop-the-newest ring eviction arm.** The newest frame is the one
  a live query most needs; discarding it converts a burst into a permanently
  stale view rather than a refetch.
- **Do not add a credit scheme to the push channel.** Credit is flow control, and
  flow control here lets a consumer stall slot confirmation.
- **Do not collapse `Unavailable` into `Null` in a cell value.** pgoutput sends
  an unchanged-TOAST marker for a large unmodified value; a consumer that cannot
  tell them apart will eventually write the wrong one into a cache and call it a
  value.
- **Do not make `Gap.reason` an open string.** That is where a physical column
  name reappears the moment someone writes a helpful error message.
- **Do not make relay readiness depend on every Datastore stream.** One broken
  tenant database would remove the only lock holder from the load balancer while
  preventing a standby from taking over.
- **Do not use one cluster-wide pending latch for topology barriers.** They must
  be per-Datastore cumulative evidence, or a failed Datastore blocks every
  healthy one.
- **Do not reuse the existing `55000 -> wal_level_not_logical` mapping on
  `START_REPLICATION`.** `55000` is `object_not_in_prerequisite_state` in
  general; slot invalidation raises it too, and the reused mapping would tell an
  operator with a full WAL disk to go set `wal_level`.
- **Do not measure advisory-lock release latency by killing a process.** Killing
  the client (even `SIGKILL`) closes its socket, so the backend sees EOF and
  releases in about a round trip - the fast path, and not the case of interest.
  `SIGKILL` on the *backend* is not a leader kill at all: the postmaster treats
  it as a crash and restarts every backend in the cluster, which would void any
  other suite sharing that server. The real case is node or network loss where no
  `FIN` is sent, so partition the network and use a dedicated cluster.
- **Do not read the projection as retroactive.** Measured: a decoder running
  across a shrink sees the pre-swap row still carrying the excluded column and
  the post-swap row not carrying it. "PostgreSQL never puts the excluded bytes on
  the wire" holds only from the swap forward, which is why a field becoming
  protected takes the destructive reset barrier rather than the ordinary bracket.
- **Do not read the `Vitess, Supabase, Salesforce, Kinesis, DynamoDB, Neon,
  Debezium and Materialize claims as evidence about this code.** They are
  relayed from a prior-art study, unverified, and cited only as prior art that
  shaped a decision. Their retention windows in particular are not targets: for
  those products retention *is* the product, whereas this ring is a fan-out
  buffer in front of a slot.
- **Do not `git mv` `wal_consumer.rs` into the relay crate.** It compiles, every
  gate in the tree stays green, and `broker::publish` then reaches a different
  process's `LazyLock` static with zero subscribers while `SuppressGuard`
  suppresses nothing in the worker. The worker keeps emitting locally and the
  relay believes it holds authority. Nothing observes this. The verdict for that
  file is "split and rewrite", and it is why the crate that exists contains no
  decode loop rather than a moved one.
- **Do not read "the crate exists" as "the relay exists".**
  `crates/zeroship-data-cdc-server` is a manifest, a config module and a `main`
  that refuses to start. Both blockers in Open 1 and Open 2 survive it untouched.
  The pressure a shell creates is to make it do something, and the cheapest way
  to do that is the move the note above forbids.
- **Do not classify the relay `test-dev-tool` to avoid the config-contract
  equality test.** It goes green and it leaves a shipped service's configuration
  surface unaudited, which is precisely the vacuity
  `crates/zeroship-config-contract/tests/real_registry.rs` exists to prevent.
  Take the `platform` class and move the pin.
- **Do not treat `zeroship-cdc-wire` as a settled foundation.** It has zero
  dependents, so no producer has ever exercised its frames, and Open 11 and Open
  12 are unresolved against it. Built, tested and unreferenced is the shape that
  looks most finished.
- **Do not trust a `file:line` in this document without re-deriving it.** The
  citation gate rules on paths and on a line existing in its file, never on
  symbols, so a rename or a sibling branch's refactor invalidates a citation
  silently. Measured examples at `a3706db6f`: `ChangeEvent` moved from
  `broker.rs` to `crates/zeroship-core/src/change_event.rs`; `ChangeStream` moved
  to `crates/zeroship-data-core/src/storage.rs`; `SUPPRESSED_APPS` and
  `emit_local` moved from `wal_consumer.rs` to `broker.rs`; and
  `replication_ops.rs` no longer exists (the watchdog dispatch is
  `crates/zeroship-plugin-db/src/v8_classes/replication.rs`).
