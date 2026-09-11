# Defect register: live defects in the code this design touches

Defects in **existing code**, found while designing and implementing the runtime
DB binding. They are not proposals and not design decisions; they are listed
together because several of them constrain the design.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled and
what blocks what. Defects that have closed, each with the commit that closed it
and the mechanism by which it stayed invisible, are in
`2026-08-26-runtime-db-binding-defects-closed.md` - that file carries the most
reusable material in the set and is not an archive.

## How to read this register

**DECIDED is not FIXED.** L1, L2 and L9 carry a decision that fixed the *shape*
of the fix. None of the three has landed; the code is still there and still
reachable. Likewise, an entry that says its subject is removed by a pending
deletion is **live until the deletion happens**.

**Deferred into the CDC service is not fixed either.** The operator decided that
a dedicated CDC service is built and every CDC defect is deferred into it rather
than patched in place (`2026-08-28-cdc-service.md`). L4, L12 and L30's
`REPLICATION` half close by construction when that service lands, and by nothing
before it.

**Treat every figure here as unverified unless the text says how it was
measured.** Where an entry names its instrument the figure is as good as the
instrument; where it does not, it is a number someone wrote down.

**Verification discipline.** State the branch, not just the SHA, and confirm the
SHA is on it with `git merge-base --is-ancestor <sha> HEAD`. A dangling SHA reads
exactly like a current one: every command that merely *reads* it succeeds and
looks authoritative. A re-verification pass run against a tree that
`git branch -a --contains` places on no branch produced three wrong verdicts
while quoting real line numbers of the wrong tree.

**Re-derive the count by listing the `### L` headings; never by adjusting the
previous count.** Every time that has been ignored the count has drifted, and a
heading whose status disagrees with its body is how a fixed entry kept reading as
live.

---

## Live defects

### L1 (DECIDED) - mask policy forgeable from any bundled dependency

Mask policy is forgeable from any bundled dependency via the global symbol
registry, and is persisted durably.

**The fix is a deletion, not a validation on the write path.** `defineMaskPolicy`,
the `Symbol.for("@zeroship/db/MaskPolicyState")` slot, the `setMaskPolicy` native
op, `dispatch_set_mask_policy` and the durable store all go; the policy arrives in
the deploy artifact beside the schema descriptor and is fixed for the isolate's
life. A guard would not have been enough: the write path is reachable from any
transitively bundled package, so any check it performs is a check on an input the
attacker also controls the timing of. Rationale in the design document, section 11.

**Evidence:** `sdks/db/src/policy.ts:87,93-94`;
`crates/zeroship-data-orm/src/protection/mask_policy.rs`

### L2 (DECIDED) - mask policy suppressible via `_flushPendingMaskPolicy`

The pending-policy drain is dynamically importable by any referrer, so a bundled
dependency can prevent the real policy landing.

L1 and L2 are one mechanism read in two directions - forge a policy, or prevent
the real one landing - and they could not have closed separately: hardening the
drain against suppression would have left forgery, and vice versa. The same
deletion closes both; there is no pending slot to drain.

**Evidence:** `sdks/db/src/internal.ts:83`; `sdks/bootstrap/src/runtime-entry.ts:196`;
`sdks/bootstrap/src/dev-entry.ts:296`;
`crates/zeroship-runtime/src/core/bootstrap_modules.rs:63`

### L3 - `__zsSchemaReady` shadowable by an accessor before assignment

An accessor installed on `globalThis` before the assignment shadows
`__zsSchemaReady`, so dispatch never awaits it.

**Evidence:** `crates/zeroship-runtime/src/core/init.rs:515-518`

### L4 - the CDC path has no mask awareness, and today that leaks column NAMES

The WAL and broker paths carry no masking concept at all. `wal_consumer.rs`
contains zero occurrences of `mask`, `wrap_row_on_read` or `apply_mask`, and
`exec.rs:531-541` builds the broker tuple from `m.iter()` over **every key** of
the `RETURNING *` row with no filtering.

**What is exposed today is names, not values, and the distinction is
load-bearing** - three successive revisions of this finding overstated it because
each traced the data to the first function that *looked* like an exit without
asking whether anything calls it.

- `ws_frame_for_change` (`broker.rs:986`) does serialise `ev.new_tuple` whole
  under `"row"`. It is reached only from `ws_frame` (`broker.rs:1024`), and
  `ws_frame` has **no callers**. It is not on any production path.
- The live creator surface is `v8_classes/subscription.rs:158` ->
  `broker::message_to_json` (`broker.rs:937-946`), which emits
  `"columns": ev.changed_columns` and **no row values**.

So the live exposure is that the changed-column list reaches creators, and it can
name `<col>_masked` today. Under the L9 storage flip it would name the raw
column. That is a requirement on the new service's wire projection, not a value
leak.

**The test that looks like it covers this is why nobody found it.**
`cdc_event_carries_masked_value_for_masked_columns` (`broker.rs:1864`) asserts a
general property in its name and rules on a ciphertext fixture:
`parent_ciphertext_text = "\\x0123456789abcdef..."`. Its own comment says what it
really guards - a regression that wires decrypt-on-CDC. For a mask-only field the
same assertion would fail today; there is simply no fixture that constructs one.
**A mask-only fixture is owed, and it is the one part of L4 that does not wait for
the service.**

**The fix is a filter, not a mask pass**, and the CDC service applies it as a
PostgreSQL publication column list so the excluded name and the excluded bytes
never reach the wire (`2026-08-28-cdc-service.md`, section 3). Masking on the way
out means the plaintext was in the frame and got transformed; filtering means the
column was never selected. The second is the one that cannot leak through a
missed call site.

### L9 (DECIDED) - the filter path is an unaudited plaintext oracle

Masking is projection-shaped, so the filter path is an unaudited plaintext
oracle. `build_where_with_dialect` takes **no schema hint**
(`zeroship-schema/src/query.rs:5244`), so it cannot know a column is masked; a
masked column stores plaintext in the parent column, so
`find({ssn:{$gt:"500-00-0000"}})` renders `WHERE "ssn" > $1` against plaintext.
The caller never sees an unmasked value and does not need to - **the matching set
is the answer**, and repeated queries binary-search the exact value with no
authorization check and no audit row. This does not violate the letter of the
audit guarantee, which covers `.unmask()` calls; it defeats the protection goal
through a channel the guarantee never scoped.

**The decided fix is to flip the storage.** `ssn` stores the MASKED value,
`ssn_raw` stores the real one, and `ssn_raw` is **reserved and unqueryable** - not
in a filter, not in a projection, not in a sort, not a field of the generated
type. Plaintext is reachable only through an explicit API, where authorization and
the audit row already live.

This beats the three policy options SC-6 had recorded, all of which kept plaintext
in the natural-named column and policed the filter path on top of it - leaving the
design fail-open, which is precisely how the defect arose. After the flip the
ignorant path is the safe path, the `"ssn_masked" AS "ssn"` substitution is
deleted rather than extended, and the guard becomes a **reserved-suffix check
needing no schema** instead of a metadata lookup the filter builder does not have.

**What the flip owes**, and none of it is optional: lookup BY plaintext must be
supported by the explicit API or the feature is closed rather than secured; unique
indexes and foreign keys must follow `ssn_raw`, since enforcing uniqueness over
masks is a silent integrity failure; and the AAD binds the column name, so this is
a migration-engine change too. Full rationale and the owed items in SC-6.

**Evidence:** `query.rs:5244` (no hint); mask substitution is select-list-only at
`query.rs:3351`, extended to aggregates by `aggregate_read_ident` at `:3412`;
audit promise at `docs/reference/db.md` "Audit tables"

### L12 - live subscriptions cost one replication slot per (app x worker), ceiling 10

`worker_slot_name` composes `__zs_slot_<sha(app)>__<sha(worker)>`
(`replication.rs:108-115`) - per pair, because a logical slot admits only one
active consumer - and `wal_consumer.rs:368` opens a **dedicated, non-pooled**
replication connection for each. `cdc_lifecycle.rs` refcounts leases per app with
**no cap on apps**. Measured on this branch's dev database:
`max_replication_slots=10`, `max_wal_senders=10`, the PostgreSQL defaults. The
11th concurrently-subscribed pair fails `pg_create_logical_replication_slot`.

`max_replication_slots` is a **restart-only** shared-memory GUC and every active
slot is a walsender backend competing for `max_connections`, so **no tuning makes
one-slot-per-tenant reach the platform's stated scale**. Slots are demand-driven -
only `collection.openSubscription()` reaches `acquire`
(`v8_classes/subscription.rs:315`) - so the bound is *concurrently subscribed*
apps, not all apps.

**Decided: the CDC service.** One process holds O(1) slots, decodes once and fans
out, so slot count stops scaling with apps and with workers, tenant filtering
happens once in a process whose only job is that, and the decode multiplier
disappears (the multiplier measurement is in the design and the index; the service
specification is `2026-08-28-cdc-service.md`). SC-3's subscription surface stays
provisional until that service lands, because it moves subscription transport out
of the worker entirely.

**Evidence:** `replication.rs:108-115`, `:208-212`; `wal_consumer.rs:368`;
`cdc_lifecycle.rs:87-111`; server GUCs measured directly

#### `max_slot_wal_keep_size` is unset, and should be set regardless

`deploy/compose/docker-compose.yml` sets `wal_level` and
`max_prepared_transactions` and nothing else. `max_slot_wal_keep_size` ships as
`boot=-1`, **unbounded**, which is what lets one abandoned slot grow `pg_wal`
until the cluster dies.

Both halves of the recommendation are measured (pg16, `tmp/measure_wal_keep_v2.sh`;
re-measured for the service design, section 7.2). It applies to a running cluster:
`ALTER SYSTEM SET max_slot_wal_keep_size` plus `pg_reload_conf()` moves the
setting with `pg_postmaster_start_time()` unchanged - no restart window, unlike
`max_replication_slots` and `max_wal_senders`, which are both
`context=postmaster`. And an over-limit slot is invalidated rather than honoured:
an idle slot went `wal_status = reserved` -> **`lost`** once WAL passed the limit,
and `pg_wal` stopped growing with the abandoned slot still present.

So the worst case becomes *that database's subscriptions resync* instead of *every
tenant on the cluster loses the server*. **It is a blast-radius cap, not a fix**:
it does not clean up an abandoned slot, and it converts a silent stall into a
subscription that must resync. It is independent of L12's outcome and of the
operator-side reaper - the reaper sweeps on an interval, so the cap is what bounds
damage inside the window before a sweep and what covers the reaper being down.

### L16 - every autocommit CRUD operation costs four round trips and a fresh parse

The single funnel at `exec.rs:310-341` does `client.transaction()` (sends
`BEGIN`), `tx.simple_query(&setup_sql)` (`SET LOCAL` role + timeouts, rebuilt per
op though it depends only on `app_id`), `tx.query_text_params(...)`, then
`tx.commit()` - **four network round trips for one `find`**. The query uses the
**unnamed** statement, so PostgreSQL parses, rewrites and plans the SQL on every
call. The driver has `prepare_cached` (`libs/compio-postgres/src/prepare.rs`), but
`statement_cache_capacity` defaults to **0**
(`libs/compio-postgres/src/config.rs:833`) and plugin-db contains **zero**
references to either name. SQLite mirrors it exactly: `backend/sqlite/session.rs`
calls `conn.prepare` twice and `prepare_cached` zero times, recompiling every
statement. All of it runs against `Pool::connect(&url, 8)`
(`crates/zeroship-data-v8/src/lib.rs`) - **8 connections per worker thread**, shared by the
~200 co-resident isolates that thread admits.

**What is measured and what is not.** The four round trips, the unnamed statement,
the zero-capacity default, the absent `prepare_cached` calls and the pool size are
all read directly from the tree. The *throughput* consequence is **not measured**.
Holding a connection longer plainly reduces ops/sec against a fixed pool of 8, but
no benchmark exists and no multiplier should be quoted until one is run.

A smaller sibling, folded here rather than given its own entry: **deprovision opens
a fresh pool per deleted app.** `deprovision_app_cdc` calls `Pool::connect(url, 2)`
(`crates/zeroship-data-v8/src/lib.rs`) on every deletion driven by the version poller - two
connects, two authentications and two TLS handshakes per app, discarded
immediately, for work that could share one long-lived platform-role pool. SC-5
names the ownership that would fix it; nothing on this branch does.

**This is the finding most directly served by the IR**, which turns a cleanup into
a design argument. A `DbPlan` has a **stable shape**: the same operation against
the same collection produces the same SQL modulo parameters. That is exactly the
precondition for a named prepared statement, and it is the property raw per-call
SQL construction throws away. An IR that renders to a cacheable statement key gets
prepare-once for free, where the current path cannot have it at any capacity
setting because nothing ever asks for a named statement. The `SET LOCAL` rebuild
has the same character: a per-binding constant recomputed per operation.

SC-3 records an overlapping measurement of the prepared-statement half with four
citations this entry does not have (`Client::new_with_statement_cache`,
`statement_cache_execution_threshold`, `bind.rs:163`, `prepare.rs:312-356`).

### L17 - a partitioned creator table is invisible to introspection, and nothing yet proves the descriptor covers it

`read_live_schema` filters `AND c.relkind = 'r'`
(`crates/zeroship-schema/src/diff.rs:641`). That predicate **includes physical (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
partitions** (a partition is `relkind = 'r'` with `relispartition = true`) and
**excludes the partitioned parent**, which is `relkind = 'p'`. So for a
partitioned creator table the parent is invisible: `build_runtime_schema` returns
`None`, and `None` means *this collection has no encrypted or masked columns -
skip the passes*. **An encrypted or masked partitioned table therefore reads with
its encryption and mask passes turned off**, because "no metadata" and "no
protection needed" are the same value.

This is not an operator-only shape: `PARTITION BY` is emitted from the public
migration DSL (`packages/zero-migrate/src/ops.ts`, lowered in
`crates/zeroship-migrate-postgres/src/ddl.rs`).

**The repository already knows the right predicate and uses it elsewhere.**
`creator_table_query` in `zeroship-migrate-server/src/publication.rs` enumerates
`relkind IN ('r','p') AND NOT c.relispartition` and excludes `__zeroship_%` by
name. Two paths in one codebase disagree about what a creator table *is*, and the
introspection path - the one that decides whether to decrypt - has the wrong
answer.

**The data plane's own introspection is gone** (decision 8; `SchemaIntrospect` for
`PostgresBackend` is now `#[cfg(any(test, feature = "test-helpers"))]` at
`backend/postgres.rs:339`), so runtime metadata comes from the descriptor
instead - which describes a partitioned table as it describes any other.

**This entry stays open for one reason: nothing has checked that.** "The new path
does not have the old path's blind spot" is an assumption until someone looks, and
the acceptance arm that would have tested it - *a relation present but
unenumerable yields `SCHEMA_INTROSPECTION_FAILED`, proved with a partitioned
table* - was retracted in the design. **This defect currently has no arm at all.**

`read_live_schema` itself is shared with the migration diff path, where
enumerating physical partitions may well be intended. Changing a shared catalog
query to serve the runtime without establishing what the migration side needs is
how a correct-looking fix breaks the other consumer.

### L19 - the worker's env cache retains decrypted secrets per app, indefinitely

`SharedEnvs` is a process-wide `HashMap<Uuid, Arc<CachedEnv>>`
(`worker/src/sync.rs:98`) holding the complete validated env JSON. Isolate LRU
eviction deliberately leaves the entry behind (`worker/src/cache.rs:780-785`), and
the only reclamation is `e.retain(|app_id, _| versions.contains_key(app_id))`
against the control plane's known-app set (`worker/src/sync.rs:216`) - so
retention tracks "every app that still **exists** and was ever loaded by this
process", not "apps with a live isolate".

**Both halves are deliberate and say so, and the leak is their composition.**
Eviction declines to touch the env because it is process-wide and another thread
may still need it; the GC exists because per-thread reconcile only fires for
locally-cached apps, so an app LRU-evicted from every thread would otherwise get
no cleanup. Each decision is locally right. The only thing that can free an entry
is an app being **deleted**; nothing frees one for an app that merely stopped
receiving traffic. The control-plane endpoint that fills it decrypts every secret
to build the response (`control/src/env_store.rs:353-390`). Stated plainly: **a
worker's plaintext-secret residency is a function of its uptime**, and deletion GC
is an eager cleanup rather than a capacity policy.

SC-5 carries the other half, and only the other half: **the GC key must include
the app incarnation**, not the app id alone. That is a contract requirement on the
service SC-5 defines, not a restatement of the leak above.

### L20 - the meter's drain is a stop-the-world, and only its growth was bounded

`Meter::drain` (`metering/src/meter.rs:257`) takes the **exclusive** lock on the
process-wide app map and holds it while iterating every tracked app, parsing a
`Uuid` per app and allocating per emitted metric. On the default ten-second cadence
(`metering/src/outbox.rs:52`) that is a periodic global pause, and every `env.db` /
`env.kv` / `env.storage` usage increment blocks behind it.

The precise bug is visible in the code's own annotations. Clippy correctly observed
the write guard is never used to mutate - the per-app drain works through interior
atomics - so the lock is taken purely as **exclusion**, and the doc comment says
why: fixed-counter `swap`s plus a `custom` take must be atomic **per app** relative
to **that app's own** increments. The stated requirement is per-app atomicity; the
implementation buys it with a process-wide exclusive lock. The fix follows from the
comment rather than contradicting it: make the exclusion per-app
(`Arc<AppCounters>` values, drained one at a time under their own guard), and the
global lock disappears with the pause.

One correction that affects the fix: the increment fast path takes a **read**
guard, and concurrent readers do not exclude one another, so
increment-versus-increment is genuinely uncontended. The contention is entirely
increment-versus-drain. Optimizing the fast path would buy nothing; only removing
the global write lock does.

**The unbounded half is fixed.** `drain` now evicts any app whose counters drained
empty, so the map - and therefore the exclusive-lock hold time, which scans that
same map - is bounded by apps with traffic since the last drain rather than by
every app the process has ever touched. `Meter::tracked_app_count` exists so the
bound can be asserted rather than trusted; regression test
`drain_evicts_apps_that_went_idle`.

**Accepted cost, do not "fix" it back:** the type promised a write lock only on
first-touch per app, and eviction means an app idle across a drain pays first-touch
again on its next increment. An app busy enough for the write lock to matter never
idles through a whole ten-second window, and one that does idle is by definition
not hot - but it IS a behaviour change, not a pure deletion.

**The stall itself was left alone on purpose.** Making the exclusion per-app is a
different change with a different risk profile; bundling it would have meant
shipping an atomicity refactor under cover of a leak fix. The unbounded *growth*
was the part that made the stall unsurvivable at target scale; the fixed-size stall
is a normal optimization arguable on its own merits.

### L21 - isolate admission builds the runtime before it can know it will be rejected

`load_app` compiles and initializes the V8 runtime first (`worker/src/cache.rs:511`)
and only then takes the cache lock to discover that a full cache whose entries are
all leased cannot evict (`:535-544`), at which point it drops the runtime it just
built. Under saturation, requests for distinct cold apps repeatedly pay a full
compile-and-evaluate for a runtime that can never be admitted - exactly the
long-tail regime the platform's stated scale implies.

**The ordering is deliberate and the guarantee must survive:** *"Only mutate the
cache after the new runtime has initialized. A corrupt descriptor during reload
must not evict the last-good isolate."* But that governs the *reload* case, where
the app is already in the cache. The wasted work is in the disjoint case - a
**new** app arriving at a full, fully-leased cache - and the two conditions that
identify it (`isolates.len() >= max_size` and `!isolates.contains_key(&app_id)`)
are both cheap and read-only; only `evict_lru` mutates. So a preflight can reject
the hopeless case before the build while leaving build-before-replace untouched for
same-app reloads. What looks like "safety ordering versus wasted work" is two
different cases sharing one code path.

### L22a - the schema map is scanned cross-tenant on every transaction start

`mint_tx_view` needs the collection names to hang off `tx.<name>`, and gets them
from `declared_collections` -> `cached_schemas_for_binding`
(`crates/zeroship-data-v8/src/context.rs`), which builds a `"{app}:{deploy}:"` prefix and
**iterates the whole thread-global schema map** to find the handful belonging to
this binding. The caller is `.map(|(name, _schema)| name)`
(`v8_classes/transaction.rs:78-81`) - the underscore is the tell: the payload
beyond the key is discarded one line later.

The scan is O(every collection of every deploy the thread has served) per
`db.transaction()`. The allocation half of this is **already fixed**: map values are
`Arc<serde_json::Value>`, so `v.clone()` is a refcount bump rather than a deep
rebuild. What is owed is a **name-only enumeration that borrows**, and per-app
sub-maps so the scan is scoped to the tenant.

The accessor is not itself a bug - its doc comment describes the shape the
drift-check sweep genuinely wants, `(collection, schema)` pairs. It is a hot path
reusing an accessor built for a cold one.

**The same shape recurs on the warm CRUD path**, in two places that survive:

- write-stage construction scans the schema **four separate times**
  (`crud/write_pipeline.rs:196-202`): `WriteStages::new` calls
  `schema_has_encrypted_columns`, `schema_has_masked_columns`,
  `schema_has_sqlite_binary_columns` and `schema_has_plain_bytes_columns` in turn,
  each walking every column, to produce four booleans one pass could yield. It
  takes `&Value`, so the fix is precomputation, not ownership.
- projection retention is `fields.iter().any(...)` inside a per-column loop
  (`crud/read_pipeline.rs:116`) - O(schema width x projection width) where a
  prebuilt set is O(width).

None of these is expensive enough to notice in a profile of a single operation,
which is exactly why they belong in a design document rather than a later
optimization pass: they are **decisions about what an accessor returns**, cheap to
make correctly now and expensive to unpick once every call site depends on owning
its result.

### L29 - the `sqlite_` reservation is on the wrong identifier role

**SQLite reserves the `sqlite_` prefix for TABLE names.** This tree fences it on
**columns** and not on tables - the inversion of the rule it is implementing.

| role | validator | fences |
| --- | --- | --- |
| collection (table) | `validate_collection`, `zeroship-schema/src/query.rs:626-664` | `pg_`, `__zeroship`. **No `sqlite_`** |
| field (column) | `RESERVED_NAMES`, `query.rs:738-766` | `_`, `__zs_`, `__zeroship_`, **`sqlite_`**, `_masked` suffix, 6 classification names |

**Why it matters here rather than in a lint:** the dev tier is SQLite, so a
creator declaring a collection named `sqlite_events` passes platform validation
and is refused by the driver instead, with SQLite's own "object name reserved for
internal use" rather than a platform message naming the rule. The failure is a
**dev-tier-only** error surfaced at the wrong layer - the shape most likely to be
reported as "the dev database is broken".

**Not a privilege escalation, and the register should not imply one.** SQLite
refuses the `CREATE` itself, so nothing is created and nothing is shadowed. The
cost is a bad diagnostic and a fence that does not mean what it says.

**Fix:** move `sqlite_` to `validate_collection` and decide deliberately whether it
stays on columns as well. `zeroship-data-sql` fences it on **both** roles and
records the divergence in its own comments; when that port lands, one of the two
behaviours has to win explicitly rather than by whichever file the reader opened.

### L30 - the worker holds REPLICATION and BYPASSRLS, which the privilege invariant forbids

The login role of the process that executes creator code holds two cluster-scoped
privileges:

```sql
ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
  INHERIT REPLICATION BYPASSRLS
-- db/migrations-ts/20260818000200_worker_database_authority.ts:35
```

Four lines below, the same file does the opposite for a role that does not need
them - `zeroship_workflow_owner ... NOINHERIT NOREPLICATION NOBYPASSRLS` (`:39`) -
so this is a deliberate grant, and the correct pattern is visible in the same file
for contrast.

**Why it is a defect and not a configuration choice.** `AGENTS.md`'s key invariant
says a privileged capability held by the process running creator code "does not
create a boundary; it creates the *appearance* of one, because everything behind
that capability is reachable by whatever reaches the worker", and that anything
genuinely privileged "belongs to a separate service that does not execute creator
code". `REPLICATION` is cluster-wide - a replication connection is confined to one
database only by the server's own decode loop, not by any grant on the role - and
`BYPASSRLS` defeats row-level security outright.

**`REPLICATION` cannot be fixed by narrowing the grant.** PostgreSQL has no finer
permission for slot consumption: `CheckSlotPermissions` is
`has_rolreplication(GetUserId())` and gates every slot function. The only remedy is
to move consumption into a process that does not execute creator code - the CDC
service. **Do not attempt a partial fix**: a "privileged slot janitor the worker
calls" is precisely the shape the invariant forbids.

**`BYPASSRLS` does not travel with the relay, and should be dropped now.** It is
unrelated to replication, so removing `REPLICATION` leaves it untouched. Measured:

- **No platform migration enables RLS.** `ROW LEVEL SECURITY` / `CREATE POLICY`
  return zero matches across `db/migrations-ts/`.
- **The declarative path cannot enable it.** `zeroship-migrate-backend` carries an
  explicit refusal: *"the desired model records no per-table RLS, so nothing here
  can render ENABLE ROW LEVEL SECURITY"* (`src/error.rs:493-512`).
- **The policy charter denies creating one.** `assert_denied("CREATE POLICY p ON
  project_acme.t USING (true)")`
  (`zeroship-migrate/crates/zeroship-migrate/tests/policy_charter/guard_security.rs:678`).

So `BYPASSRLS` on `zeroship_worker` bypasses nothing today - it is dead privilege.
**That is precisely why it should go now.** RLS is unused, not unreachable: the IR
path has a `setRls` op and a `safety.require_rls` obligation, named in the refusal
above. The day any app enables RLS, a worker holding `BYPASSRLS` silently ignores
it - a tenant-isolation bypass arriving with no code change and no error, in a
process that executes creator code. Removing a privilege that currently does
nothing is free; removing it after something depends on it is a behaviour change.

### L31 - Bulk-write limits do not cover the ordinary update and purge paths

Status: open; verified against the current ORM and SQL compiler.

`run_update_many` in
[CRUD execution](../../crates/zeroship-data-orm/src/crud/mod.rs) probes target keys
and rejects targets above `MAX_QUERY_LIMIT` only when
`per_row_encrypted_update` is true. An update that does not need per-row
encryption goes directly to `build_update_many_with_assignments` in the
[SQL compiler](../../crates/zeroship-data-sql/src/compile.rs). That builder adds a
`WHERE` clause only for a nonempty filter and imposes no row limit. Its
`RETURNING` projection contains declared readable fields, but still materializes
all matching rows.

`plan_purge_many` delegates to `build_delete_many`, which likewise has no target
cap. Removing implicit column behavior did not address these limits.

The regression test
`update_many_randomised_target_cap_rejects_without_writes_sqlite_runtime` in
[SQLite update tests](../../crates/zeroship-data-v8/src/tests/sqlite/updates.rs)
exercises the encrypted branch. It does not establish a bound for ordinary
updates or purges. Resolving this finding requires enforcing the intended limit
on those paths and testing that an oversized operation leaves rows unchanged.

### L32 - the migration-freeze guard reports instead of failing, and its data source has no writer

`AGENTS.md` devotes its longest passage to one rule: a migration file a deployed
database has applied is FROZEN; edit it and every later run against that database
aborts permanently. The pre-flight guard that detects such an edit is now advisory,
and the table it reads is no longer written by anything.

- **It reports.** `deploy/scripts/deploy-remote.sh:1113-1122` prints
  `note  N file(s) differ from the frozen legacy journal` rather than calling
  `fail()` (which exists at `:91` and is used elsewhere in the same script). A
  mismatch prints and the roll continues.
- **Its source has no producer.** The check reads
  `zeroship_migrations.platform_migration_files` (`:643`, `:656`). That table's only
  writer was the `zeroship-platform-migrate` binary, deleted with the
  `zeroship-migrate-adapter` crate. The `zero-migrate` CLI that replaced it keeps a
  different journal (`schema_migrations`, keyed by migration version, checksummed
  over rendered SQL rather than file bytes) and never touches this table. Its 34
  recorded checksums are all stale against the `schema()`-form corpus: 34 of 34
  files differ.

**Demoting the arm was a deliberate, correct call, not an oversight** - enforcing
the byte comparison would fail every roll with 34 false "edited after applied"
reports, which is worse. It is recorded here because the *consequence* outlived the
reasoning: the freeze discipline currently rests on nothing mechanical, and the CI
half is gone too (it was a test in the adapter crate, deleted with it).

**Two things still stand and should not be confused with this one.**
`released_ledger_misordered` (`deploy-remote.sh:260`, enforced at `:1133`) is a
different check - it catches a NEW file that sorts before an applied one - and it
now matters *more*, because the retired runner derived journal versions from file
ordinal and aborted on the collision while the CLI derives them from the migration
name, so a mid-corpus insert applies with no error at all. This script is the only
thing that sees it.

**Not fixed, and the fix is a decision rather than a patch.** Either the CLI
re-baseline writes `platform_migration_files`, restoring the producer, or
`db/released_migrations.tsv` is retired and the freeze check moves to the engine's
own journal, which `journal_sql::applied`
(`migrate-postgres/src/backend/journal_sql.rs:506`) can already read and no
platform service calls. `2026-08-28-migration-record-consolidation.md` specifies
the second, which is the better end state.

### L33 - SQLite vector search cannot work on any migration-produced database

`.search()` on a `t.vector()` field fails with "no such table" on the dev tier,
on any database an actual migration produced. It has never worked there.

The `vec0` shadow relation the search JOIN targets is **authored by nothing**:

- the SQLite renderer folds a derived ANN index to a plain B-tree and drops the
  opclass - `project_derived_ann_index` sets `access_method = "btree"` and
  `opclass = None` (`zeroship-migrate-sqlite/src/schema.rs:98-105`);
- the engine states outright that it **never authors a virtual table**
  (`zeroship-migrate-backend/src/error.rs:270`);
- the runtime descriptor nevertheless *names* the relation
  (`AuxiliaryObject::ShadowTable`, `migrate-core/src/render/gen_types.rs:219`,
  `:255`), and `vector_search`'s JOIN depends on that name.

**Why it stayed invisible is the useful part.** The only thing that ever created
the relation was `ensure_vector_index` in the data plane - which was
`#[cfg(any(test, feature = "test-helpers"))]`, so it ran in tests and in no
shipped binary. The tests therefore passed against a relation the tests
themselves created, on a code path production could not reach. Deleting that
function (`ac38fac0e`) did not cause this; it removed the thing that hid it.

**One property already works in its favour:** the engine's drop pass refuses to
cascade a live `vec0` virtual table away (`DropOfVirtualTable`,
`error.rs:291`), so a migration that *does* create one will not be undone by the
next diff. Only the authoring half is missing.

**PostgreSQL is not proven either, for a different reason.** Every PG search
test is `#[ignore]`d or self-skips on an image without pgvector and PostGIS -
and `spatial_near_runs_under_per_app_role_via_rls` reports `ok` via an internal
skip, which is a green that ruled on nothing. Verifying the PG side needs a
pgvector+PostGIS image and `--ignored`.

---

## Not a defect: a missing gate arm

### L7 - the sanitization rail's diagnostic reaches a subscriber in 2 of 9 test binaries

The diagnostic the sanitization rail relies on ("diagnosable only from a worker
log", `dispatch.rs:245-246`) went to a discarded stream because no integration
binary installed a subscriber, so `RUST_LOG` had nothing to configure.
`support::init_test_tracing` (`crates/zeroship-data-v8/tests/support/mod.rs`) now exists and is
called by `native_transaction.rs` and `distributed_live.rs`. Installing it
immediately surfaced the cause of four opaque failures: `permission denied for
schema default`, from a `DROP SCHEMA ... CASCADE` in the harness that destroyed the
per-app grants without restoring them.

Coverage is **2 of 9 binaries**. The remaining seven (`capability`, `db_v8_class`,
`integration`, `missing_role`, `platform_fence`, `sqlite_integration`,
`subscription_finalizer`) are the gate arm.

Worth recording because it is corroboration rather than coincidence: that harness
bug is the **same failure mode** this proposal documents for restore - `DROP SCHEMA
CASCADE` destroys grants and `ALTER DEFAULT PRIVILEGES` entries, and whatever
recreates the schema must restore them. It was found from a completely different
direction.

---

## Closed since this register was written

Entries whose subject the tree no longer contains. Kept as one-liners so a
cross-reference from another document resolves rather than dangling.

- **L17's cache-bound siblings, L18** - the cross-tenant `introspected_schemas`
  map, `crud/introspect_schema.rs` and `live_metadata.rs` are deleted with the data
  plane's introspection (decision 8). The per-populate cap and singleflight that
  were L18's partial fix went with them, and the flat-map bound it still owed is
  moot.
- **L22b** - the SQLite actor's cross-app transaction refusal now has a code of its
  own, `transaction_lanes_exhausted`
  (`backend/sqlite/session.rs:113`), whose doc comment names L22b and states the
  reason: the two remedies are opposite and only one is the creator's to act on.
  `transaction_connection_busy` now means "*this* app already has a transaction
  open" in both producers.
- **L28** - `platform-cli` did not compile and blocked 26 test harnesses. The
  feature and the `zeroship-migrate-adapter` crate that carried it are gone. An
  example of why defect repair waits for the refactor rather than racing it; L32 is
  its successor.
- **The v3 `get_column_key` finding** (numbered L9 in v3; unrelated to the live L9
  above) - `install_get_column_key_function` no longer exists. Decision 1 replaced
  per-column keys with one key per app derived
  `HKDF(platform_master_key, app_id, key_version)`, so the constraint on how to
  rebuild the function is moot.
- **The `pitr_targets` PUBLIC grant** - `ensure_pitr_targets_table` no longer
  exists; PITR target selection moves to the control plane. **Residual:**
  `Backup::pitr_replay` still inserts into `__zeroship_admin.pitr_targets`
  (`backend/postgres.rs:1732`), a table nothing now creates
  (`backend/mod.rs:1373`), and is to be deleted with it.
- **The `SECURITY DEFINER` slot wrappers** - `install_slot_wrapper_functions` no
  longer exists. It was only ever called by the test suite despite a comment
  claiming the V8 callback layer called it too, so there was never a V8-reachable
  privileged path. Slot and publication ownership belongs to the CDC service.
