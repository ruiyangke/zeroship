# SC-5: service ownership

**Date:** 2026-08-26

**Status:** required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`. Step 5a - the
behaviour-neutral half of the cutover, which mints the service and moves plugin
and operator-lifecycle ownership onto it - is implemented. **SC-5 as a contract
is at zero**: Fork C's identity, the ceiling as a service field, and
service-owned key custody are unbuilt.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## Why this exists

The parent proposal describes an isolate-owned `DbIsolateBinding` holding
`Rc<DbThreadResources>` resolved from something that outlives every isolate.
Nothing in the worker had that lifetime, and two concrete facts show the shape
of the gap:

- **The plugin set is memoised per THREAD, which is not process-wide.**
  `plugin_set()` mints the vector on first use and returns clones of the same
  `Arc`s on every later call (`crates/zeroship-worker/src/cache.rs:236-244`),
  and `build_runtime` calls it (`cache.rs:463`). But the slot it memoises into
  is a `thread_local!` (`PLUGIN_SET`, `cache.rs:228`), so an n-thread worker
  still holds n plugin sets and the memo cannot be the process-wide owner.
- **Operator deprovisioning opened its own database.** The free function that
  handled a deleted app's CDC teardown took a `&str` URL, re-ran
  `backend_for_url` on it and built a fresh two-connection `Pool` **per deleted
  app** - a second backend selection and a second pool, parallel to whatever the
  worker already had. `crates/zeroship-plugin-db/src/lib.rs:1027-1030` records
  the shape it replaced; the work now runs through the service's lifecycle
  handle.

A backend slot that must be shared across a thread, and configuration that must
be decided exactly once for the process, cannot be anchored in something scoped
to one thread and minted by whoever gets there first.

## The contract

One `Arc<DbService>`, constructed once at worker/CLI composition, before any
isolate exists. It owns:

| Owned | Why it cannot live per-runtime |
| --- | --- |
| Validated configuration (`DbPluginConfig`) | Backend selection must happen once, not per isolate; the parent proposal forbids a second URL parse |
| The plugin prototype | So `build_runtime` **clones an `Arc`** instead of minting a plugin set |
| The stable thread-resource key | So current and deploy-pinned isolates on one OS thread resolve the *same* `DbThreadResources` |
| The neutral operator-lifecycle handle | So deprovision uses the service's backend selection and one shared operator pool rather than reparsing the URL and opening a pool per deletion |
| The operator mask ceiling | It is operator configuration, immutable per isolate, and every binding meets it with that deploy's creator draft - so it must exist before any binding does |
| The platform master key | Column keys are derived `HKDF(platform_master_key, app_id, key_version)`, so the key is service-level configuration rather than a per-app lookup |

**The service owns no live-metadata cache.** The design's no-introspection rule
leaves the data plane with nothing to introspect, so there is no per-operation
metadata to share; `live_metadata.rs` and every consumer that existed to hold it
are deleted. Anywhere this contract once spoke of a process-wide *cache entry*,
the process-wide object that remains is the service's own `Arc`s.

The ceiling and the master key are listed here because "constructed once at
worker/CLI composition, before any isolate exists" is exactly the lifetime each
needs: a value that arrives any later reopens the staleness problems they exist
to remove.

`DbService` is `Send + Sync` because it crosses worker-thread boundaries, but it
holds only validated configuration and `Sync` data - never a driver connection.
`factory.open()` still runs on the owning worker thread and yields an
`Rc<dyn DbBackend>`; nothing becomes `Send` merely to satisfy service storage.

### What this contract does not specify, and nothing else in the set does either

- **The ceiling's configuration source and format.** "Worker configuration"
  names the worker's composition point; `zeroship serve` and the Vite dev vector
  are separate composition points, and SC-4 does not cover them. A dev tier with
  no ceiling source, combined with SC-6's "failure is denial", denies every
  non-`auto` unmask in dev permanently.
- **Where the platform master key comes from for this crate.**
  `zeroship-core`'s `PLATFORM_SECRETS` table
  (`crates/zeroship-core/src/config/secrets.rs:88-129`) has no row for a
  database column-key master, and `ZEROSHIP_CONTROL_MASTER_KEY` is by its name
  the control plane's. Reusing it puts one secret in two trust domains - the
  control plane and the process that executes creator code - which is the shape
  `AGENTS.md`'s "privilege follows the PROCESS" invariant warns about. A new
  named platform secret with its own floor is the shape that fits; naming it is
  not this document's call.

### One requirement this contract states about code it does not own

**The GC identity of a per-app cache must include the incarnation.** The
worker's env cache is reclaimed by app id alone
(`e.retain(|app_id, _| versions.contains_key(app_id))`,
`crates/zeroship-worker/src/sync.rs:216`), so a same-id recreation retains the
previous app's env snapshot and serves it to the new one until an `env_version`
bump displaces it. That is a stale-secret leak rather than an untidy cache, and
it is the same identity gap Fork C closes for DB handles - a fence on the DB
door while the env door stays open is a fence around the wrong door. The defect
belongs to another subsystem and is recorded in the defect register as L19; what
belongs here is that the incarnation this contract mints is the identity that GC
must key on.

## Lifecycle: a deprovision arriving while an isolate holds a handle

- A deprovision is routed through the service's lifecycle handle, so it uses the
  same backend selection the data plane uses - not a second pool.
- It **does not** invalidate handles already cloned into live isolates. Those
  hold `Rc`s and may have futures in flight; yanking the backend underneath them
  would turn an operator action into a data-plane crash.
- Instead it marks the app **deprovisioned**, so no *new* operation resolves
  resources for it, and existing operations fail closed at their next authority
  read - which finds the **tombstone** and denies terminally. The row stays and
  its state changes; a deleted row is not a tombstone, and deleting it removes
  the only durable evidence that this app id was ever deprovisioned, which is
  precisely how a PITR rewind or a recreated id reopens the cross-incarnation
  hole.
- The thread-resource entry is dropped when its last holder releases it, which
  is the existing `Rc` semantics rather than a new mechanism.

## Fork C: the durable AppIncarnationId

This is the fork the parent proposal names `Fork C` ("what fences a stale
handle") and defines by pointing back here.

**The storage is decided: two layers, because no single home delivers the four
properties.**

1. **A control-owned, append-only lifecycle ledger, in a recovery domain
   independent of the application cluster.** It holds immutable incarnation
   history, tombstones, and a CAS-controlled head. Recreation appends
   incarnation B; it never removes incarnation A's tombstone. This is the
   canonical authority.
2. **A worker-read-only projection beside the application data**, written only
   by the lifecycle service and read with a plain `SELECT` before any data SQL.
   This is the one shape the system schema is for - a separate service writes,
   the worker only reads - and it gives the worker no privileged operation and
   no `SECURITY DEFINER` capability.

**The projection is an enforcement cache, not the authority.** It co-rewinds
with the application data, which is the point: a control-only home can revert to
incarnation A while the app cluster holds B's data, and an A-handle then passes
the compare and reaches B's data. Neither layer alone is sufficient, and the two
cross-check each other's rewinds.

The control plane is the right logical owner, but **not because app lifecycle
already lives there** - that argument engages none of the four properties. It is
right because control does not execute creator code, so it is the correct
process to mint and to CAS. It is *wrong* if it means another row in today's
`zeroship` schema: control, auth and application data share one cluster today,
so a whole-cluster restore rewinds the record and the data it fences in the same
instant. Creator-schema storage is categorically disqualified, because the
migrator owns that schema.

**Consequently the app must be unavailable to workers after any restore until
the projection is reconciled from the external ledger under a newly minted,
out-of-band authority generation.** This is the cost, and it is not optional:
without an external witness the four properties are unachievable, as the
measurement below shows.

**These are not schema metadata**, which is why the descriptor cannot absorb
them and why the deferred DDL-validation feature would not cover them either:
two incarnations of one app id have the **same** schema, so any comparison of
catalog against descriptor passes for both. What Fork C fences is identity, not
shape.

### The tombstone is permanent

A marker keyed on `app_id` alone and **cleared on recreation** reauthorizes a
handle cloned before the deprovision: if the recreated schema is
descriptor-compatible, that handle reaches the **new** app's data. The binding
identity carries `app_id`, `deploy_hash` and `runtime_instance_id`, so without
an incarnation nothing distinguishes the two.

The objection to spending an incarnation on this is that same-id recreation is
not an operation the platform offers. That is true of the ordinary creation path
and does not generalise. App ids really are database-minted there:
`zeroship.apps.id` is `t.uuid().notNull().default(uuidV4())`
(`db/migrations-ts/20260702000200_control_tables.ts:106-108`), the creation path
inserts `(name, plan_id, api_key, api_key_hash)` `RETURNING id` supplying no id
(`crates/zeroship-control/src/registry.rs:245-251`), and deletion is a hard
`DELETE FROM zeroship.apps WHERE id = $1` (`registry.rs:369`) with no
soft-delete column and no restore path. **But "the ordinary creator API does not
supply an id" is not "an id can never recur."** Operator tooling, direct
provisioning, test fixtures (which already insert explicit ids) and above all
**PITR** sit outside that path.

PITR is what settles it, and it is this proposal's own threat model. The parent
binds authority to **`(system_identifier, timeline_id)`** rather than
`system_identifier` alone, because same-cluster PITR preserves the latter and
`timeline_id` is what moves when recovery rewinds and promotes; it also records
that a dump rewinds the per-app platform tables. A tombstone written into a
rewindable table is therefore itself rewindable, and a rewound tombstone is no
tombstone: the deprovisioned app's authority row returns with no creator API
having reused anything. A fence that a routine operator recovery can erase is
not a fence.

### The domain does not see every rewind

**Measured on PostgreSQL 17.11**, two runs differing in exactly one variable -
whether `recovery.signal` was present:

| recovery shape | `system_identifier` | `timeline_id` | data |
| --- | --- | --- | --- |
| SIGKILL, restart (crash recovery) | unchanged | **1, unchanged** | replayed |
| basebackup + `recovery.signal` + promote | unchanged | **2** | rewound |

A new timeline is created only when **archive** recovery completes. Crash
recovery replays whatever WAL is on disk and comes up on the same
`(system_identifier, timeline_id)`. So restoring a filesystem, EBS, ZFS or LVM
snapshot and starting the server **without** `recovery.signal` rewinds the data
and leaves the authority domain byte-identical - and that is the most common
cloud restore shape.

The paragraph above sets the standard this fails: *"A fence that a routine
operator recovery can erase is not a fence."* The domain qualification defends
the promoting recoveries and is blind to the non-promoting ones.

Two consequences the contract must state rather than imply:

1. **The tombstone property is bounded, not absolute.** "Never deleted" holds
   only against rewinds the domain can observe. Against a snapshot restore that
   bypasses archive recovery, **no database-resident record survives anywhere** -
   not in the app cluster, not in the control plane, because restoring either
   rewinds that side's own tombstone. The honest statement of property 1 is
   "never deleted, except by a restore that bypasses archive recovery", carried
   with an operator rule that restores use `recovery.signal`. A witness outside
   every rewind domain - an append-only ledger mirrored to the blob store - is
   the only construction that would make the absolute form true.
2. **`pg_upgrade` moves the domain with no rewind at all.** It builds a fresh
   `initdb` cluster, and a fresh cluster mints a fresh `system_identifier`
   (measured: two independently initialised containers report
   `7679151263737716786` and `7678445743677390892`). Because the incarnation
   comparison is terminal with no re-resolution, a routine major-version upgrade
   would permanently deny every app. The contract needs a deliberate, audited
   **re-domain ceremony**, or the platform can never upgrade PostgreSQL.

A **logical restore into the same existing cluster** moves neither component
either, and is a second same-domain rewind.

### How the domain must be observed

Three rules, because the obvious reading of each is wrong:

1. **Take the timeline from the WAL filename, not the control file.**
   `pg_control_checkpoint().timeline_id` reports the timeline recorded in the
   latest *completed* checkpoint, and promotion requests a spread checkpoint
   rather than an immediate one, so the value can lag the very event it exists
   to detect. `pg_walfile_name(pg_current_wal_lsn())` is constructed from the
   current WAL *insertion* timeline; its leading eight hex digits are the
   timeline (measured on PG 16.14: `000000010000000000000020`).
   `pg_split_walfile_name` is available on PG 16 (verified present in `pg_proc`)
   and parses it.
2. **Fail closed whenever `pg_is_in_recovery()` is true.** A recovery paused at
   its target, or a standby never promoted, serves read-only queries of rewound
   data on an unchanged timeline. `recovery_target_action` defaults to `pause`
   precisely so that state can be inspected, so this is a default configuration,
   not an exotic one.
3. **`pg_control_system()` carries no timeline at all** - verified, it returns
   `pg_control_version`, `catalog_version_no`, `system_identifier` and
   `pg_control_last_modified`. The pair must be assembled from two sources, and
   a reader that assumes one call yields both will silently compare a constant.

**Timeline ids are not globally unique branch identifiers.** A promoting server
picks the locally newest timeline plus one, so two replicas promoted in
isolation from the same ancestor can both choose the same number while
diverging. The pair is useful provenance; it is not an authority generation.

### The shape, therefore

1. An opaque 128-bit `AppIncarnationId`, minted **by the privileged server side,
   never chosen by a caller**. It is persisted with the app's lifecycle state,
   carried in every binding, and compared before any data SQL.
2. **It is qualified by the authority domain `(system_identifier, timeline_id)`,
   or PITR simply resurrects an old token along with the row that holds it.**
   The incarnation inherits the parent's PITR defence rather than reopening the
   hole one field over. This is what makes the token worth adding at all.
3. **A tombstone is never cleared.** Deprovision leaves it; recreation mints a
   new incarnation rather than erasing the old marker.
4. **The comparison is terminal and the value is its own.** An incarnation
   mismatch denies permanently, with no re-resolution, and no deploy-, schema-
   or metadata-versioning value may be reused to carry it: a value that legally
   changes under a migration either revokes healthy bindings when pinned or
   follows the recreated app when followed. The parent's error contract keeps
   `APP_DEPROVISIONED`, `STALE_APP_INCARNATION` and `AUTHORITY_DOMAIN_MISMATCH`
   distinguishable for the same reason.

Any home for this state must preserve four properties, which are the ones the
deleted design paid for: a durable tombstone that is never deleted, a CAS
against an expected incarnation, an authority domain a restored dump cannot
assert about itself, and three distinguishable error codes. **The first is
bounded rather than absolute** unless the ledger of layer 1 sits outside every
rewind domain; see the measurement above.

### Dev carries a different type, not a pretend incarnation

The dev tier has no control plane to mint from, no PostgreSQL recovery domain,
and no deprovision or recreate operation; SC-2 removes the file-resident
authority row outright. A binding is therefore an explicit sum type:

- **Production (PostgreSQL)** - a durable `AppIncarnationId`, an externally
  anchored authority generation, and live cluster observation.
- **Dev (SQLite)** - a process-local attach generation, minted by the connection
  owner and changed on detach or reattachment.

Do **not** reuse the HMR supervisor generation: it changes for code-lifecycle
reasons, not for deprovision or recreation. Do **not** mint a per-process random
value either - dev workflow records persist across restarts, so a per-process
token would deny replay of every dev workflow after an ordinary restart, which
is precisely what a developer testing durability is trying to exercise.

The cost is that dev cannot exercise `APP_DEPROVISIONED`,
`STALE_APP_INCARNATION` or `AUTHORITY_DOMAIN_MISMATCH`; those need PostgreSQL
integration tests. That is the truth about the tier and is worth stating rather
than simulating.

**One dev edge is open.** The dev workflow journal is a separate database from
the app's - `.zeroship/workflows.sqlite` against `.zeroship/dev.sqlite`
(`crates/zeroship-cli/src/main.rs:334-340`). Deleting or replacing the app file
therefore leaves an old workflow eligible to replay against the new one. Dev
needs an atomic reset policy, where the workflow journal resets with the app
database, or a supervisor-owned project lifecycle generation.

## Acceptance shape

### What every arm below states

Set-up, execution, and the observable that separates pass from fail - plus the
answer to **where does this go red**. An arm with no answer to the third is not
written down here. The seven ways a green run rules on nothing are in
`2026-08-26-runtime-db-binding-verification-record.md`; two of them bite this
contract's territory hard enough to be named at the arms where they bite.

### The two labels, and the rule about converting between them

- **Buildable** - nothing outside this contract blocks the arm. Some are already
  in the tree and are named with their test.
- **Blocked: `<thing>`** - the arm needs something that does not exist. The named
  thing is a second cluster, the lifecycle ledger, the projection, or an operator
  ceremony.

**A blocked arm is never narrowed until it passes.** Narrowing is how a fixture
comes to exclude the state its name is about, which is class 7. The arm most
exposed to it here is the restore fence: its only single-fixture form is the one
restore shape the domain can see, which is why 3.6 is a matrix and not a test.

No placeholder discharges a blocked identity arm. A bare app id, a deploy hash
or a lazily minted token is exactly the identity the fence exists to distinguish
from, so an arm rewritten onto one passes on the hole it was written to close.
`AppIncarnationId` occurs 0 times in the tree.

### The fence's shape is settled; the axis it is keyed on is not

A decoupling is under consideration in which one app holds several databases and
several apps share one. Under it the thing needing a fence is the **database**,
or the app-to-database grant, and not the app. The consequences for this list:

- **Group 2 is unaffected**, and this is a second reason to build it first. Those
  arms are facts about PostgreSQL recovery rather than about apps: a rewind that
  leaves `(system_identifier, timeline_id)` byte-identical does so whoever owns
  the database, and a paused standby serves rewound data to any reader.
- **Group 3 is written against "the binding's identity"** - durable,
  privileged-minted, tombstoned, domain-qualified, terminal on mismatch - rather
  than against a per-app incarnation. `A` and `B` name two identities of one
  lifecycle entity; *which* entity is the open question. No arm here should grow
  an app-keyed fixture more elaborate than that, because the fixture is the part
  that would be thrown away.
- **Two arms are app-axis by nature and stay app-axis whatever is decided**: 3.5,
  because an env snapshot belongs to an app and to no database, and 3.4, because
  the pending set is the worker's own per-app teardown queue.
- **One arm inverts**, and it is flagged in place at 1.4.

### The invocations

Group 1 and 3's worker-side arms run under `cargo test -p zeroship-worker --lib`
and `cargo test -p zeroship-plugin-db --lib`. The database arms run under

    RUST_MIN_STACK=33554432 \
    cargo test -p zeroship-plugin-db --test integration --features test-helpers \
      -- --test-threads=1

Five of this crate's test targets declare `required-features`, so a plain
`cargo test -p zeroship-plugin-db` filters them out unbuilt and prints a smaller
green. Group 2's arms need a fixture that can create, crash, snapshot, promote
and restore a cluster; no target in the tree does that today, and the arm that
introduces it owes its own invocation line.

### The same-thread sharing arm is a preservation property, not evidence

It passes on code that predates this contract. The pool and the backend live in
the thread-local DB context (`THREAD_DB_CTX` / `ThreadDbContext`,
`crates/zeroship-plugin-db/src/context.rs`; cite the symbol, this declaration has
moved twice in two weeks), so two isolates on one OS thread have always shared
them and minting a fresh `DbPlugin` per `build_runtime` never produced two
backends. Keep it green, never cite it for service ownership. Arm 1.1 is the
discriminating partner it needs.

The general rule behind that, worth applying before writing any arm here: an
acceptance criterion stated as an absolute (`no`, `never`, `exactly one`) is a
claim about what the implementation can reach, so ask *on which thread, holding
what*.

---

## Group 1 - ownership. Buildable except 1.4's second half; three are in the tree

### 1.1 One plugin prototype across two OS worker threads

**Buildable, and in the tree:**
`db_plugin_prototype_is_one_object_across_worker_threads`,
`crates/zeroship-worker/src/cache.rs:1111`.

- **Set up:** one `DbService`, handed to two OS threads.
- **Executed:** each thread resolves the db plugin **through its own context**,
  the way `build_runtime` does.
- **Observable:** `Arc::ptr_eq` over the two `Arc<dyn NativePlugin>`, with the
  parent holding both alive at the moment of comparison. Each thread keeps its
  own driver resources.
- **Red when:** the prototype is minted per thread.

Two traps, both found by 5a's mutation run and both invisible in review:

- **Each thread must RESOLVE its own handle.** Handing both threads one cloned
  `Arc` and comparing it with itself cannot fail - it passes on a per-thread
  implementation too.
- **Compare the `Arc`s, not their addresses.** Returning
  `Arc::as_ptr(..) as usize` from each thread and comparing the integers reported
  EQUAL under the mutation that reintroduces per-thread prototypes: a thread's
  plugin set is a `thread_local!` dropped at thread exit, so the allocator handed
  the second thread the address the first had just freed. Holding both `Arc`s
  alive while comparing makes address reuse impossible. An arm comparing raw
  addresses across a thread boundary measures the allocator, not the design.

Two threads, not two isolates on one thread. A same-thread form passes
identically on a wrong implementation that keeps one service per thread, because
`THREAD_DB_CTX` is thread-local and both isolates share it either way.

### 1.2 `build_runtime` selects no backend and opens no pool

**Buildable, and in the tree:**
`building_the_plugin_set_selects_no_backend_and_opens_no_pool`,
`crates/zeroship-worker/src/cache.rs:1005`.

- **Set up:** a composed service.
- **Executed:** `build_runtime`, not `plugin_set`. Stopping at the plugin set
  leaves the path that actually re-mints unmeasured.
- **Observable:** the URL-parse counter and the pool-open counter are both
  unchanged across the build; the plugin arrives as a clone of the service's
  `Arc`.
- **Red when:** the build re-runs backend selection or `Pool::connect`.

### 1.3 Operator deprovisioning: one pool per batch, no second parse, release proved

**Buildable, and in the tree:**
`operator_deprovisioning_reuses_one_pool_and_reparses_nothing`,
`crates/zeroship-plugin-db/tests/integration.rs:3161`. The production release
call is `crates/zeroship-worker/src/sync.rs:205`.

- **Set up:** a composed service and **three** deletions. With one, "one pool per
  deletion" and "one pool ever" are the same number and the arm rules on nothing.
- **Executed:** three `deprovision_app` calls, then `close_operator_pools`, then
  a fourth deletion.
- **Observable:** the parse counter is unchanged from composition; the pool-open
  delta across the batch is exactly 1; the delta after the release is exactly 2.
- **Red when:** deprovision reverts to a `Pool::connect` per call (the pool
  assertion), or to a second backend selection on the URL (the parse assertion),
  or `close_operator_pools` is not installed in the poller loop (the release
  assertion).

**"No second pool" is not satisfiable and must not be written.** Deprovision runs
on the version-poller thread, which hosts no isolate and therefore has no
data-plane pool to reuse, so "no pool" can only be met by not connecting at all.
What the clause protects against is a pool **per deletion** - two connects, two
authentications and two TLS handshakes for every deprovisioned app. One memoised
pool per reconcile batch delivers that (`OPERATOR_POOL_SIZE = 2`,
`crates/zeroship-plugin-db/src/service.rs:77`).

**The fourth deletion is the whole release half.** If the count stays at one, the
release did nothing. `close_operator_pools` (`service.rs:451`) began as a
`#[cfg(test)]` helper, which is what made *the pool is never closed* true of every
shipped binary while looking handled in the tests - the same shape as L12b, where
the abandoned-slot reaper existed but was reachable only from tenant JS. A release
path with no production caller is a capability, not a release path. The release
must also be called where a runtime can still drive shutdown: dropping a `Pool`
asks its detached driver tasks to close, it does not wait for them.

**Accepted cost, deliberately taken:** a later batch of deletions pays a fresh
connect rather than the process holding two idle backends from its first deletion
until exit. Do not "fix" this by keeping the pool installed.

### 1.4 One `DbThreadResources` per thread per resolved identity

Two halves, and only one of them is buildable.

**Buildable half (cardinality).** Current and deploy-pinned isolates resolving the
same identity on one worker thread resolve **one** `Rc<DbThreadResources>`.

- **Observable:** `Rc::ptr_eq`, with backend factory opens counted separately.
  Not a connection count: a pool of two and two pools of one are indistinguishable
  from the server, so a connection count cannot rule on map cardinality.
- **Red when:** a deliberately duplicated resolution is introduced. The arm must
  be shown to fail against that mutation or it is measuring nothing.
- **`Rc`, not `Arc`.** `DbThreadResources` is held as `Rc<DbThreadResources>`
  (parent, the `AppDbBinding` struct) and driver resources stay non-`Send`
  deliberately, because they never cross a thread. Pointer checks in this contract
  are therefore over two different smart pointers, and the asymmetry is the
  design: per-thread resources under `Rc`, process-wide service state under `Arc`.
  An arm using `Arc` for both is a compile error dressed as a cross-thread
  guarantee.
- The type has no occurrences in the tree, so this arm is buildable and has no
  subject yet.

**Blocked: the lifecycle ledger and projection (discrimination).** Two identities
of one lifecycle entity, on one thread, resolve **different** resources. This is
the half that matters, and the cardinality half passes without it on an
implementation keyed by app id alone.

**This is the arm that inverts if apps and databases decouple.** Today "one app on
one thread" and "one database on one thread" are the same sentence, which is what
makes an app-keyed fixture look adequate. Under a shared database they are
opposites: two apps on one thread must share **one** `DbThreadResources`, and one
app with two databases must hold **two**. An app-keyed map expresses neither.
Write the fixture against the resolution key rather than against app ids; the
`Rc::ptr_eq` observable is axis-free and survives either outcome.

### 1.5 The initialization singleflight

**Buildable; blocked on nothing external, and on nothing but its own mechanism.**
`zeroship-plugin-db` contains no singleflight today.

- **Set up:** two isolates on one worker thread, neither having performed a DB
  operation.
- **Executed:** both issue their first operation, interleaved so both reach
  resolution before either publishes.
- **Observable:** exactly one backend is opened, and both isolates observe the
  same `Rc`.
- **Red when:** the singleflight is deleted. **That mutation is the only
  acceptable evidence.** `THREAD_DB_CTX` is thread-local, so the naive form of
  this arm passes with no singleflight at all; writing it before the mechanism
  produces a green with nothing behind it.

---

## Group 2 - the authority domain reader. Buildable ahead of Fork C

The reader observes `(system_identifier, timeline_id)` from the live cluster.
Nothing in production reads it today. **Every arm in this group needs only the
reader and a recovery fixture** - not the ledger, not the incarnation, not a
lifecycle service - which is why this group is worth building first: it pins the
domain's blind spots before anything depends on them.

### 2.1 The restore matrix

**Buildable.** One fixture per restore shape, run as a matrix. A single-shape
fixture here is the class-7 failure, and rows 1, 2 and 4 are the shapes a
single-shape fixture omits.

| # | restore shape | data | expected observation |
| --- | --- | --- | --- |
| 1 | SIGKILL, restart (crash recovery) | replayed | pair **unchanged** |
| 2 | filesystem/EBS/ZFS/LVM snapshot, started without `recovery.signal` | rewound | pair **unchanged** |
| 3 | basebackup + `recovery.signal` + promote | rewound | `timeline_id` **+1**, `system_identifier` unchanged |
| 4 | logical restore of a dump into the same running cluster | rewound | pair **unchanged** |

- **Observable:** the reader's reported pair, recorded per row.
- **Red when:** the timeline comes from a source stable across promotion (row 3
  goes red), or from something that varies with an ordinary restart such as a
  checkpoint LSN (rows 1, 2 and 4 go red).
- **Three of the four rows expect NO change, and that is the point.** They pin the
  disclosed blind spot so that a later change which appears to close it fails
  loudly instead of quietly redefining what the fence covers. A matrix run only on
  row 3 reports a working fence over three shapes where the domain contributes
  nothing.

### 2.2 Fail closed while `pg_is_in_recovery()` is true

**Buildable, and the sharpest arm in this group.**

- **Set up:** a recovery with `recovery_target_action = pause` - the default -
  stopped at its target, serving read-only queries of **rewound** data on an
  **unchanged** pair.
- **Executed:** an authority read.
- **Observable:** the read denies. It does not adopt a domain and does not report
  a match.
- **Paired control, in the same test:** the same standby promoted, where the read
  must SUCCEED. Without the control the arm is satisfied by a reader that denies
  everything, which is the SC-6 shape: a deny-only arm passes on a total
  functional break.
- **Red when:** the reader compares the pair only. That implementation passes all
  four rows of 2.1 and fails here, which is what makes this arm worth its fixture.

**Second assertion, same fixture:** an **unbound** binding must not ADOPT a domain
observed while `pg_is_in_recovery()` is true. The parent's 3.3 permits the first
read to adopt; a first read against a paused standby would adopt a rewound
cluster's domain as authoritative and every later comparison would agree with it.

### 2.3 The timeline comes from the WAL filename

Two parts, and only the first discriminates.

**Part A, deterministic and buildable.** The reader is given a WAL filename and a
control-file timeline that **disagree** - `000000020000000000000003` against `1` -
and must return 2.

- **Red when:** the reader takes `pg_control_checkpoint().timeline_id`, or
  assembles the pair from `pg_control_system()` alone, which carries no timeline
  at all and would make the reader compare a constant.

**Part B, live and non-forcing.** At the first read after `pg_is_in_recovery()`
turns false following `pg_promote(wait := false)`, both the WAL-derived timeline
and `pg_control_checkpoint().timeline_id` are recorded; the arm fails if the
WAL-derived value is not the promoted timeline.

- **This arm cannot force the lag.** Promotion requests a spread checkpoint, but
  the measured run reached an end-of-recovery checkpoint and read the new timeline
  immediately. Part B detects the lag when it occurs and is **not** evidence about
  the choice of source. Recording only part B and calling it "the WAL source is
  verified" is the class-2 shape.

### 2.4 `pg_upgrade` is not a terminal denial

**Blocked: a second PostgreSQL major-version binary in the fixture, and the
re-domain ceremony, which this contract names and no document specifies.**

- **Set up:** a cluster with a live app, `pg_upgrade`d to the next major.
  `system_identifier` changes with **no rewind at all** (measured: two
  independently initialised clusters report `7679151263737716786` and
  `7678445743677390892`).
- **Executed:** an authority read before the ceremony, the audited re-domain
  ceremony, then a read after.
- **Observable:** before, `AUTHORITY_DOMAIN_MISMATCH`; after, the app serves.
- **Both halves are required.** The denial half alone is satisfied by a platform
  that can never upgrade PostgreSQL, which is the failure this arm exists to
  prevent, so it must not be landed on its own.

**The contract sentence this arm depends on:** "terminal, with no re-resolution"
is a property of a **binding**. The ceremony mints a new authority generation and
new bindings; it does not re-resolve an existing one. Without that distinction
this arm and the terminal rule contradict each other.

### 2.5 The pair is not an authority generation

**Blocked: two clusters in one fixture; the second assertion additionally on the
ledger.**

- **Set up:** two standbys from one basebackup, promoted in isolation from the
  same ancestor.
- **Executed:** the reader against each.
- **Observable:** both report the **same** `(system_identifier, timeline_id)`
  while holding divergent data. The ledger's authority generation must differ
  across the two.
- **This arm expects agreement**, and its value is that it pins the property: any
  later change that treats the pair as a branch identifier fails here.

---

## Group 3 - identity. Blocked on the lifecycle ledger and its projection

Layer 1 (the control-owned append-only ledger, in its own recovery domain) and
layer 2 (the worker-read-only projection) both have to exist before any arm here
can run. Where an arm has a half that runs today, the half is named. `A` and `B`
are two identities of one lifecycle entity, per the axis note above; every arm
here is stated so that the entity can change without the arm changing.

### 3.1 Two codes, two moments, one handle

**Blocked: the ledger and the projection.**

- **Set up:** an isolate holding a handle for identity A; the entity retired, so
  the ledger holds A's tombstone and the lifecycle service has written the
  projection.
- **Executed:** (a) an operation on the stale handle while the entity is still
  retired; then (b) the entity re-provisioned as identity B, and the same handle
  used again.
- **Observable:** (a) `APP_DEPROVISIONED`; (b) `STALE_APP_INCARNATION`. Both
  terminal, neither retried.
- **Red when:** the tombstone is cleared on recreation - which turns (a) into a
  success and (b) into a success, because a descriptor-compatible recreated schema
  is reachable by the A handle - or when the two codes are collapsed into one.
- An arm asserting only "denies" cannot tell (a) from (b), and the audit trail
  needs to distinguish "it is gone" from "it came back without you".

**The distinction is axis-free; the two code names are not.** What the arm rules
on is a fence bearing the handle's *own* identity against a fence bearing a
successor's, which is the same observable whether the entity is an app or a
database. If the axis moves, the codes are renamed and this arm is untouched.

### 3.2 A deprovision does not abort an in-flight operation

**PostgreSQL only.** The no-abort half is buildable today; the fail-closed half is
**blocked on the projection**.

- **Set up:** an isolate with an operation in flight.
- **Executed:** a deprovision arrives through the service's lifecycle handle; the
  in-flight operation is allowed to run to completion; a new operation is then
  issued for the same app.
- **Observable:** the in-flight operation returns its own result unaffected (this
  half runs today: deprovision uses the operator pool and touches no data-plane
  pool); the next operation is denied at its authority read.
- **Red when:** deprovision yanks the backend from under live `Rc` holders, which
  turns an operator action into a data-plane crash.

Scoped to PostgreSQL deliberately: SC-2's `ReattachFile` **must** settle or abort
outstanding reservations and detach both connections before restore's file swap,
so an unscoped no-abort rule and SC-2 are jointly unsatisfiable on SQLite.
Graceful deprovision and forced file detach are different events.

### 3.3 A durable run created under A does not replay against B

**Blocked: the ledger, plus three record changes that do not exist.**

- **Set up:** a workflow run created and made **durable under identity A, before B
  exists**. That ordering is the entire arm: a run created after the recreate
  carries B correctly on any implementation, including one that resolves the
  identity at replay time rather than persisting it, so a same-identity fixture
  cannot tell the two apart.
- **Executed:** retire, re-provision as B, then attempt replay of the A run.
- **Observable:** replay is refused and B's pinned isolate is never entered.
- **Red when:** the identity is resolved at claim or replay time instead of being
  persisted at creation. **A mutation replacing the persisted token with a fresh
  lookup must turn this red**, and that mutation is the arm's only proof, because
  the two implementations are indistinguishable on any run created after B.
- **The records that must change first:** `CandidateRun` carries `app_id`,
  `deploy_id` and `deploy_hash` and no lifecycle token
  (`crates/zeroship-plugin-workflow/src/claim.rs:38-52`), its claim query selects
  exactly those (`claim.rs:83`), and the pinned key is
  `PinnedWorkflowKey { app_id, deploy_hash }`
  (`crates/zeroship-worker/src/cache.rs:29-32`).

**Carrying the identity on the version poll does not discharge this.** The poll
delivers the *current* identity; once B exists it returns B, and nothing can
reconstruct that an already-durable journal belongs to A. Everything else in this
contract fences a handle against the present; this is the one case that must fence
it against the past.

*Axis note: a run is created by an app and replays against a database, so under
the decoupling the token persisted at creation is the identity of what the run
BINDS TO, not of the app that started it. The arm is unchanged; the column it
needs is on the same record either way.*

### 3.4 The worker's delayed deprovision does not act on the successor

**Blocked: the ledger, and a pending set that can carry a token.**

- **Set up:** an app deleted and its id entering the poller's pending set; the app
  recreated as B before the pending entry drains.
- **Executed:** the pending cleanup runs.
- **Observable:** the cleanup is skipped rather than applied to B's resources.
- **Red when:** the pending entry is a bare id. It is today - the set is
  `HashSet<Uuid>` (`crates/zeroship-worker/src/sync.rs:137`, drained at
  `:163-167`) - so there is nothing to compare and the arm has no subject.

### 3.5 A same-id recreation does not serve the previous app's env snapshot

**Blocked: the identity the GC must key on. Buildable today as a failing
reproducer, and it cannot go green before Fork C. App-axis by nature: an env
snapshot belongs to an app and to no database, so the decoupling does not move
it.**

- **Set up:** an app with a cached env snapshot; its id leaves the control plane's
  version map and reappears.
- **Executed:** the version poller's GC pass, then an env read for the new app.
- **Observable:** the new app does not observe the old snapshot.
- **This arm goes red on today's tree**, which is unusual in this list and is the
  reason to keep it. `e.retain(|app_id, _| versions.contains_key(app_id))`
  (`crates/zeroship-worker/src/sync.rs:216`) reclaims by app id alone, so the
  previous app's snapshot is retained until an `env_version` bump displaces it -
  a stale-secret leak, registered as L19. Another subsystem's code, the same
  identity gap, and a fence on the DB door while the env door stays open is a
  fence around the wrong door.

### 3.6 A restore does not resurrect a retired entity's bindings

**Blocked: the ledger; rows 1, 2 and 4 additionally on the reconcile procedure.**

This arm is a matrix over the same four restore shapes as 2.1, and **the expected
observable differs by row**. The single-row form - promote, observe the timeline
moved, declare the fence working - is the class-7 shape: its fixture is the one
restore shape where the domain moves, so it reports a working fence over three
shapes where the domain contributes nothing.

- **Row 3 (promoting restore).** The domain moved. A binding bound to the
  pre-restore pair denies with `AUTHORITY_DOMAIN_MISMATCH`.
  - **Red when:** the identity is unqualified by the domain. The arm must be shown
    to fail against a bare token, or it is testing the token and not the defence.
- **Rows 1, 2 and 4 (same-domain rewinds).** The entity is **unavailable to
  workers** until the projection has been reconciled from the ledger under a newly
  minted, out-of-band authority generation.
  - **Observable:** a worker read denies before reconciliation and succeeds after,
    and the generation minted differs from the pre-restore one.
  - **Red when:** the implementation lets a worker proceed on an unchanged pair
    after a restore, which is precisely what the domain alone permits.
  - **No database-resident record on either side distinguishes these rows** -
    restoring either cluster rewinds that side's own tombstone - so the ledger is
    the only witness, and this half cannot be softened into something a single
    cluster can answer.

### 3.7 The tombstone survives, bounded

**Blocked: the ledger, present in the fixture in its own recovery domain.**

The property is not "never deleted". It is: never deleted by any operation the
platform performs, and never deleted by a recovery that reaches archive recovery;
**deleted by a restore that bypasses archive recovery**, which is why 3.6's
reconcile exists.

- **Set up:** a tombstone appended for identity A, then a recreation appending B,
  which must not remove A's tombstone.
- **Executed:** the four restore rows of 2.1, each applied to the **application
  cluster**, plus a fifth row applying a restore to the **ledger's own** cluster.
- **Observable:** rows 1 to 4 leave the ledger holding A's tombstone and B's head;
  row 5 is the one that may lose it, and its expected outcome is that the app
  cluster's projection refuses to serve rather than trusting a rewound ledger.
- **Red when:** the ledger shares a recovery domain with the app cluster. **A
  fixture that restores only the app cluster cannot see this** - it proves nothing
  about independence, it assumes it.

### 3.8 The head moves by CAS

**Blocked: the ledger.**

- **Set up:** a ledger head at identity A.
- **Executed:** two concurrent recreate attempts, both expecting A.
- **Observable:** exactly one appends B; the other is refused and appends nothing.
- **Red when:** the head is a last-writer-wins update, under which both attempts
  report success and one identity is lost.

The head is per lifecycle entity, so the decoupling changes how many heads the
ledger holds and not what this arm asserts about one of them.

---

## Group 4 - dev. Buildable once the sum type exists

Nothing external blocks these. Dev cannot exercise `APP_DEPROVISIONED`,
`STALE_APP_INCARNATION` or `AUTHORITY_DOMAIN_MISMATCH`, for the reasons under
"Dev carries a different type"; those need the PostgreSQL arms above.

**This group is already on the database axis and the decoupling does not touch
it.** The attach generation is minted by the connection owner and changes on
detach or reattachment, so it fences a *file*, never an app. That is a point in
favour of the sum type rather than a coincidence: the tier that was forced to
name what it actually fences named the database.

### 4.1 An ordinary dev restart does not deny replay

- **Set up:** a dev workflow made durable, then the dev process restarted against
  the same `.zeroship/dev.sqlite`.
- **Observable:** the workflow replays.
- **Red when:** the attach generation is a per-process random value. That
  implementation satisfies every other arm in this group and breaks exactly what a
  developer testing durability is trying to exercise.

### 4.2 An HMR reload does not change the attach generation

- **Set up:** a handle taken, then a code reload.
- **Observable:** the handle still resolves.
- **Red when:** the HMR supervisor generation is reused as the attach generation.
  It changes for code-lifecycle reasons, not for deprovision or recreation.

### 4.3 Replacing the app file does change it

- **Set up:** a handle taken, then a detach and reattach, or the app file
  replaced.
- **Observable:** the old handle denies.
- **Red when:** the generation is minted once per process, which is 4.1's
  implementation seen from the other side. **4.1 and 4.3 must be run as a pair**;
  either alone is satisfied by an implementation the other rejects.

### 4.4 A dev workflow does not replay against a replaced app database

**Blocked: a policy decision, not code.** The journal is a separate database -
`.zeroship/workflows.sqlite` against `.zeroship/dev.sqlite`
(`crates/zeroship-cli/src/main.rs`, the `workflow_db_path` default) - so deleting
or replacing the app file leaves an old workflow eligible to replay against the
new one. The arm cannot be written until the contract chooses between an atomic
reset policy and a supervisor-owned project lifecycle generation: under the first
the observable is an empty journal, under the second it is a denied replay, and
the two fixtures are different.
