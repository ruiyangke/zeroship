# SC-5: service ownership

**Status.** PARTIAL. The ownership half ships in
`crates/zeroship-plugin-db/src/service.rs` (`DbService`, `DbLifecycle`) and
`crates/zeroship-worker/src/cache.rs` (the shared plugin prototype). The identity
half - Fork C, the authority-domain reader, the ceiling and the master key as
service fields - is unbuilt: `AppIncarnationId`, `DbThreadResources` and
`attach_generation` occur zero times in `crates/`, and Fork C has no home that
satisfies its own requirement (Open 1).

---

## What it is

### The service

One `Arc<DbService>`, constructed once at worker/CLI composition, before any
isolate exists. It is `Send + Sync` because it crosses worker-thread boundaries,
and the static `assert_send_sync::<DbService>()` (`service.rs:270-271`) is also
the mechanical proof it holds no driver connection: `Pool` is `!Send`, so it
cannot appear in any field. Backend opens still run on the owning worker thread
and yield an `Rc<dyn DbBackend>`; nothing becomes `Send` to satisfy service
storage.

Owned today (`service.rs:259-263`):

| Owned | Why it cannot live per-runtime |
| --- | --- |
| The validated URL and selected `BackendUrl` | Backend selection happens once. `select_backend` has exactly one production call site, `service.rs:291`; every other occurrence is `#[cfg(test)]` or `test-helpers` |
| `DbResourceKey` (`service.rs:193`) | A 32-byte digest of the URL, not the URL, because it reaches `Debug` output. It is the identity current and deploy-pinned isolates on one OS thread resolve against |
| `Arc<DbPlugin>`, the plugin prototype | So `build_runtime` (`cache.rs:482`, calling `plugin_set()` at `:503`) clones an `Arc` instead of minting a plugin set |
| The parsed operator system-shape charter | `DbService::new` calls `system_shape_charter::load()`, so a malformed authority fails at composition rather than inside the first write |
| The operator-lifecycle handle (`DbLifecycle`, `service.rs:352-362`) | So deprovision uses the service's backend selection and one shared operator pool rather than reparsing the URL and opening a pool per deletion |

Specified, not built:

| To be owned | Why it needs this lifetime |
| --- | --- |
| The operator mask ceiling | Operator configuration, immutable per isolate, met by every binding against that deploy's creator draft - so it must exist before any binding does. `MaskCeiling` exists as a reducer type (`transaction/reducer/identity.rs:130`) but is not a service field |
| The platform master key | Column keys are derived `HKDF(platform_master_key, app_id, key_version)`, which makes the key service-level configuration rather than a per-app lookup. No `master_key` or HKDF derivation exists in `crates/zeroship-plugin-db/` or the `zeroship-data-*` crates |
| `DbThreadResources` | The per-thread resource bundle the parent's `AppDbBinding` holds under `Rc`. The type does not exist; today the thread's pool and backend live in `THREAD_DB_CTX` / `ThreadDbContext` (`context.rs:362`) |

**The service owns no live-metadata cache.** The design's no-introspection rule
leaves the data plane with nothing to introspect, so there is no per-operation
metadata to share. There is no `live_metadata.rs`. Where this contract speaks of
a process-wide object, it means the service's own `Arc`s.

### One requirement about code this contract does not own

**The GC identity of a per-app cache must include the incarnation.** The worker's
env cache is reclaimed by app id alone - `e.retain(|app_id, _|
versions.contains_key(app_id))` (`crates/zeroship-worker/src/sync.rs:216`) - so a
same-id recreation retains the previous app's env snapshot and serves it to the
new one until an `env_version` bump displaces it. That is a stale-secret leak,
registered as L19. The defect belongs to another subsystem; what belongs here is
that the incarnation this contract mints is the identity that GC must key on. A
fence on the DB door while the env door stays open is a fence around the wrong
door.

### Lifecycle: a deprovision arriving while an isolate holds a handle

- A deprovision routes through `DbLifecycle::deprovision_app`
  (`service.rs:381`), so it uses the service's backend selection and this
  thread's memoised operator pool (`operator_pool`, `service.rs:400`), not a
  second pool.
- It **does not** invalidate handles already cloned into live isolates. Those
  hold `Rc`s and may have futures in flight; yanking the backend underneath them
  turns an operator action into a data-plane crash.
- Instead it marks the app **deprovisioned**, so no *new* operation resolves
  resources for it, and existing operations fail closed at their next authority
  read - which finds the **tombstone** and denies terminally. The row stays and
  its state changes. A deleted row is not a tombstone: deleting it removes the
  only durable evidence that this app id was ever deprovisioned, which is how a
  PITR rewind or a recreated id reopens the cross-incarnation hole.
- The thread-resource entry is dropped when its last holder releases it - the
  existing `Rc` semantics, not a new mechanism.

**The trigger is out-of-band today.** The worker enqueues a deprovision when an
app id leaves the control-plane version feed
(`pending_cdc_deprovision`, `sync.rs:137`). The ordinary creator API no longer
removes an app row: retirement is `UPDATE zeroship.apps SET archived_at =
COALESCE(archived_at, NOW())` (`crates/zeroship-control/src/registry.rs:380-388`),
reversible at `:477-485`, and `get_versions` (`registry.rs:690`) does not filter
on `archived_at`, so an archived app stays in the feed. The deprovision path is
therefore reachable only by a row actually leaving the feed - operator tooling,
direct provisioning, or a restore.

### Fork C: the durable app incarnation

This is the fork the parent proposal names `Fork C` ("what fences a stale
handle") and defines by pointing here.

**The shape.**

1. An opaque 128-bit `AppIncarnationId`, minted **by the privileged server side,
   never chosen by a caller**. Persisted with the lifecycle state, carried in
   every binding, compared before any data SQL.
2. **Qualified by the authority domain `(system_identifier, timeline_id)`**, or
   PITR resurrects an old token along with the row that holds it. The incarnation
   inherits the parent's PITR defence rather than reopening the hole one field
   over. This is what makes the token worth adding at all.
3. **A tombstone is never cleared.** Deprovision leaves it; recreation mints a new
   incarnation rather than erasing the old marker.
4. **The comparison is terminal and the value is its own.** A mismatch denies
   permanently with no re-resolution, and no deploy-, schema- or
   metadata-versioning value may be reused to carry it: a value that legally
   changes under a migration either revokes healthy bindings when pinned or
   follows the recreated app when followed. The parent's error contract keeps
   `APP_DEPROVISIONED`, `STALE_APP_INCARNATION` and `AUTHORITY_DOMAIN_MISMATCH`
   distinguishable for the same reason.

**The consuming side is built; the producing side is not.** The reducer's
`AuthorityIdentity`, `AuthorityDomain`, `SchemaEpoch`, `LifecycleState`,
`MaskCeiling`, `classify` and all three `Verdict` arms exist and are tested
(`crates/zeroship-data-engine/src/transaction/reducer/identity.rs`). The single
production construction site mints incarnation 0, domain `(0, 0)`, epoch 0,
`Stable` and an empty ceiling, and `observation_for` echoes the expectation back
(`crates/zeroship-data-engine/src/transaction/driver.rs:141-165`, which says so in
its own doc comment), so `classify` can only return `Current` in production and
the three typed denial codes are unreachable outside tests. Fork C is the input
that machine is waiting for; `driver.rs`'s two functions are the ones that change.

**Storage is two layers, because no single home delivers the four properties.**

1. **An append-only lifecycle ledger in a recovery domain independent of the
   application cluster.** Immutable incarnation history, tombstones, a
   CAS-controlled head. Recreation appends incarnation B; it never removes A's
   tombstone. This is the canonical authority. **It has no home - see Open 1.**
2. **A worker-read-only projection beside the application data**, written only by
   the lifecycle service and read with a plain `SELECT` before any data SQL. This
   is the one shape a system schema is for - a separate service writes, the worker
   only reads - and it gives the worker no privileged operation and no
   `SECURITY DEFINER` capability.

**The projection is an enforcement cache, not the authority.** It co-rewinds with
the application data, which is the point: an authority-only home can revert to
incarnation A while the app cluster holds B's data, and an A-handle then passes
the compare and reaches B's data. Neither layer alone suffices; the two
cross-check each other's rewinds.

**Consequently the app is unavailable to workers after any restore until the
projection is reconciled from the ledger under a newly minted, out-of-band
authority generation.** This is the cost and it is not optional: without an
external witness the four properties are unachievable.

**These are not schema metadata**, which is why the descriptor cannot absorb them
and why DDL validation would not cover them: two incarnations of one app id have
the **same** schema, so any comparison of catalog against descriptor passes for
both. Fork C fences identity, not shape.

**The four properties any home must preserve**: a durable tombstone that is never
deleted, a CAS against an expected incarnation, an authority domain a restored
dump cannot assert about itself, and three distinguishable error codes. The first
is **bounded rather than absolute** unless layer 1 sits outside every rewind
domain.

### How the authority domain is observed

Three rules, because the obvious reading of each is wrong.

1. **Take the timeline from the WAL filename, not the control file.**
   `pg_control_checkpoint().timeline_id` reports the timeline of the latest
   *completed* checkpoint, and promotion requests a spread checkpoint rather than
   an immediate one, so the value can lag the very event it exists to detect.
   `pg_walfile_name(pg_current_wal_lsn())` is built from the current WAL
   *insertion* timeline; its leading eight hex digits are the timeline (measured
   on PG 16.14: `000000010000000000000020`). `pg_split_walfile_name` is present in
   `pg_proc` on PG 16 and parses it.
2. **Fail closed whenever `pg_is_in_recovery()` is true.** A recovery paused at
   its target, or a standby never promoted, serves read-only queries of rewound
   data on an unchanged timeline. `recovery_target_action` defaults to `pause`
   precisely so that state can be inspected - this is default configuration, not
   an exotic one.
3. **`pg_control_system()` carries no timeline at all.** It returns
   `pg_control_version`, `catalog_version_no`, `system_identifier` and
   `pg_control_last_modified`. The pair must be assembled from two sources; a
   reader assuming one call yields both silently compares a constant.

**Timeline ids are not globally unique branch identifiers.** A promoting server
picks the locally newest timeline plus one, so two replicas promoted in isolation
from the same ancestor can choose the same number while diverging. The pair is
useful provenance; it is not an authority generation.

**The domain does not see every rewind.** Measured on PostgreSQL 17.11, two runs
differing in exactly one variable - whether `recovery.signal` was present:

| recovery shape | `system_identifier` | `timeline_id` | data |
| --- | --- | --- | --- |
| SIGKILL, restart (crash recovery) | unchanged | **1, unchanged** | replayed |
| basebackup + `recovery.signal` + promote | unchanged | **2** | rewound |

A new timeline is created only when **archive** recovery completes. Restoring a
filesystem, EBS, ZFS or LVM snapshot and starting **without** `recovery.signal`
rewinds the data and leaves the domain byte-identical - and that is the most
common cloud restore shape. A logical restore of a dump into the same running
cluster is a second same-domain rewind.

Two consequences the contract states rather than implies:

1. **The tombstone property is bounded.** "Never deleted" holds only against
   rewinds the domain can observe. Against a snapshot restore that bypasses
   archive recovery, **no database-resident record survives anywhere** - not in
   the app cluster, not in the ledger's cluster, because restoring either rewinds
   that side's own tombstone. The honest form is "never deleted, except by a
   restore that bypasses archive recovery", carried with an operator rule that
   restores use `recovery.signal`. Only a witness outside every rewind domain - an
   append-only ledger mirrored to the blob store - makes the absolute form true.
2. **`pg_upgrade` moves the domain with no rewind at all.** It builds a fresh
   `initdb` cluster, and a fresh cluster mints a fresh `system_identifier`
   (measured: two independently initialised containers report
   `7679151263737716786` and `7678445743677390892`). Because the comparison is
   terminal with no re-resolution, a routine major-version upgrade would
   permanently deny every app. A deliberate, audited **re-domain ceremony** is
   required, or the platform can never upgrade PostgreSQL (Open 5).

### Dev carries a different type, not a pretend incarnation

The dev tier has no control plane to mint from, no PostgreSQL recovery domain,
and no deprovision or recreate operation; SC-2 removes the file-resident
authority row outright. A binding is therefore an explicit sum type:

- **Production (PostgreSQL)** - a durable `AppIncarnationId`, an externally
  anchored authority generation, and live cluster observation.
- **Dev (SQLite)** - a process-local attach generation, minted by the connection
  owner and changed on detach or reattachment.

Dev cannot exercise `APP_DEPROVISIONED`, `STALE_APP_INCARNATION` or
`AUTHORITY_DOMAIN_MISMATCH`. Those need PostgreSQL integration tests. That is the
truth about the tier and is worth stating rather than simulating.

**The dev tier's proposed fence is on the database axis; its built code is not.**
The attach generation fences a *file*. What ships is app-keyed exactly like
production: `SqliteBackend::attach_app_file` takes an `app_id`, derives
`zs-{app_id}.sqlite`, aliases the attachment by app id and dedups on
`app_id_cache` (`crates/zeroship-data-sqlite/src/lib.rs:680-704`, field at
`:167`). One file per app, one app per file. If an app may hold several
databases, the file name, the alias and the cache key can no longer all be the
app id.

### Acceptance

Every arm states its set-up, its observable, and **where it goes red**. An arm
with no answer to the third is not written here. The seven ways a green run rules
on nothing are in `2026-08-26-runtime-db-binding-verification-record.md`; the
traps each arm has already fallen into are in History, not repeated per arm.

**A blocked arm is never narrowed until it passes.** Narrowing is how a fixture
comes to exclude the state its name is about. **No placeholder discharges a
blocked identity arm**: a bare app id, a deploy hash or a lazily minted token is
exactly the identity the fence exists to distinguish from, so an arm rewritten
onto one passes on the hole it was written to close.

**Invocations.** Worker-side arms run under `cargo test -p zeroship-worker --lib`
and `cargo test -p zeroship-plugin-db --lib`. The database arms run under

    RUST_MIN_STACK=33554432 \
    cargo test -p zeroship-plugin-db --test integration --features test-helpers \
      -- --test-threads=1

Five of this crate's test targets declare `required-features`, so a plain
`cargo test -p zeroship-plugin-db` filters them out unbuilt and prints a smaller
green. Group 2 needs a fixture that can create, crash, snapshot, promote and
restore a cluster; no target in the tree does that, and the arm that introduces
it owes its own invocation line.

**The axis is not settled** (Open 6). Group 2 is unaffected - its arms are facts
about PostgreSQL recovery, true whoever owns the database. Group 3 is written
against "the binding's identity" (durable, privileged-minted, tombstoned,
domain-qualified, terminal on mismatch), with `A` and `B` naming two identities of
one lifecycle entity; no arm should grow an app-keyed fixture more elaborate than
that, because the fixture is the part that gets thrown away. Two arms are app-axis
by nature and stay so: 3.5 (an env snapshot belongs to an app and to no database)
and 3.4 (the pending set is the worker's own per-app teardown queue). One arm
inverts: 1.4.

An acceptance criterion stated as an absolute (`no`, `never`, `exactly one`) is a
claim about what the implementation can reach, so ask *on which thread, holding
what*.

#### Group 1 - ownership

**1.1 One plugin prototype across two OS worker threads.** SHIPPED:
`db_plugin_prototype_is_one_object_across_worker_threads`
(`crates/zeroship-worker/src/cache.rs:1192`). One `DbService` handed to two OS
threads; each **resolves the db plugin through its own context**, the way
`build_runtime` does; `Arc::ptr_eq` over the two `Arc<dyn NativePlugin>` with the
parent holding both alive at the moment of comparison. Red when the prototype is
minted per thread. Two threads, not two isolates on one thread: a same-thread form
passes identically on an implementation keeping one service per thread, because
`THREAD_DB_CTX` is thread-local and both isolates share it either way.

**1.2 `build_runtime` selects no backend and opens no pool.** SHIPPED:
`building_the_plugin_set_selects_no_backend_and_opens_no_pool`
(`cache.rs:1086`). Execute `build_runtime` (`cache.rs:482`), not `plugin_set` -
stopping at the plugin set leaves the path that actually re-mints unmeasured. The
URL-parse and pool-open counters are both unchanged across the build; the plugin
arrives as a clone of the service's `Arc`. Red when the build re-runs backend
selection or `Pool::connect`.

**1.3 Operator deprovisioning: one pool per batch, no second parse, release
proved.** SHIPPED: `operator_deprovisioning_reuses_one_pool_and_reparses_nothing`
(`crates/zeroship-plugin-db/tests/integration.rs:3637`); the production release
call is `crates/zeroship-worker/src/sync.rs:205`. **Three** deletions, then
`close_operator_pools`, then a fourth. The parse counter is unchanged from
composition; the pool-open delta across the batch is exactly 1; the delta after
the release is exactly 2. Red when deprovision reverts to a `Pool::connect` per
call, or to a second backend selection on the URL, or when `close_operator_pools`
is not installed in the poller loop.

**"No second pool" is not satisfiable and must not be written.** Deprovision runs
on the version-poller thread, which hosts no isolate and so has no data-plane pool
to reuse; "no pool" can only be met by not connecting at all. The clause protects
against a pool **per deletion** - two connects, two authentications and two TLS
handshakes per deprovisioned app. One memoised pool per reconcile batch delivers
that (`OPERATOR_POOL_SIZE = 2`, `crates/zeroship-plugin-db/src/service.rs:77`).
The release must be called where a runtime can still drive shutdown: dropping a
`Pool` asks its detached driver tasks to close, it does not wait for them.

**1.4 One `DbThreadResources` per thread per resolved identity.** Two halves.

- *Cardinality.* BUILDABLE, no subject yet - the type has zero occurrences.
  Current and deploy-pinned isolates resolving the same identity on one worker
  thread resolve **one** `Rc<DbThreadResources>`. Observable: `Rc::ptr_eq`, with
  backend factory opens counted separately. **Not a connection count**: a pool of
  two and two pools of one are indistinguishable from the server. Red under a
  deliberately duplicated resolution - the arm must be shown to fail against that
  mutation or it measures nothing.
- *Discrimination.* BLOCKED on the ledger and projection. Two identities of one
  lifecycle entity, on one thread, resolve **different** resources. This is the
  half that matters; the cardinality half passes without it on an implementation
  keyed by app id alone.

**`Rc`, not `Arc`.** Driver resources stay non-`Send` deliberately, because they
never cross a thread. Pointer checks in this contract are over two different smart
pointers and the asymmetry is the design. An arm using `Arc` for both is a compile
error dressed as a cross-thread guarantee.

**This is the arm that inverts if apps and databases decouple.** Today "one app on
one thread" and "one database on one thread" are the same sentence, which is what
makes an app-keyed fixture look adequate. Under a shared database they are
opposites: two apps on one thread must share **one** `DbThreadResources`, and one
app with two databases must hold **two**. Write the fixture against the resolution
key; the `Rc::ptr_eq` observable is axis-free.

**1.5 The initialization singleflight.** SHIPPED. The mechanism is
`ThreadDbContext::begin_backend_init` / `finish_backend_init`
(`crates/zeroship-plugin-db/src/context.rs:253-268`), claimed at
`crates/zeroship-plugin-db/src/lib.rs:1222`; the arm is
`concurrent_sqlite_lazy_init_shares_one_backend` (`lib.rs:1423`). Eight concurrent
cold inits open exactly one backend; remove `begin_backend_init` and it reports 8.
It is also the liveness proof for `service::backend_open_count`, which 1.2 asserts
did NOT move - a counter wired to nothing satisfies 1.2 forever.

#### Group 2 - the authority domain reader

Nothing in production reads the domain. **Every arm needs only the reader and a
recovery fixture** - not the ledger, not the incarnation, not a lifecycle service -
which is why this group is worth building first: it pins the domain's blind spots
before anything depends on them.

**2.1 The restore matrix.** BUILDABLE. One fixture per restore shape, run as a
matrix.

| # | restore shape | data | expected observation |
| --- | --- | --- | --- |
| 1 | SIGKILL, restart (crash recovery) | replayed | pair **unchanged** |
| 2 | filesystem/EBS/ZFS/LVM snapshot, started without `recovery.signal` | rewound | pair **unchanged** |
| 3 | basebackup + `recovery.signal` + promote | rewound | `timeline_id` **+1**, `system_identifier` unchanged |
| 4 | logical restore of a dump into the same running cluster | rewound | pair **unchanged** |

Red when the timeline comes from a source stable across promotion (row 3 reds), or
from something that varies with an ordinary restart such as a checkpoint LSN (rows
1, 2 and 4 red). **Three of four rows expect NO change, and that is the point.**
They pin the disclosed blind spot so a later change that appears to close it fails
loudly instead of quietly redefining what the fence covers.

**2.2 Fail closed while `pg_is_in_recovery()` is true.** BUILDABLE, and the
sharpest arm in this group. A recovery with `recovery_target_action = pause` - the
default - stopped at its target, serving read-only queries of **rewound** data on
an **unchanged** pair. An authority read must deny: it does not adopt a domain and
does not report a match. **Paired control, same test:** the same standby promoted,
where the read must SUCCEED. Red when the reader compares the pair only - that
implementation passes all four rows of 2.1 and fails here, which is what makes
this arm worth its fixture. **Second assertion, same fixture:** an **unbound**
binding must not ADOPT a domain observed while `pg_is_in_recovery()` is true. The
parent's 3.3 permits the first read to adopt; a first read against a paused
standby would adopt a rewound cluster's domain as authoritative and every later
comparison would agree.

**2.3 The timeline comes from the WAL filename.** Two parts; only the first
discriminates.

- *Part A, deterministic, BUILDABLE.* The reader is given a WAL filename and a
  control-file timeline that **disagree** - `000000020000000000000003` against `1`
  - and must return 2. Red when it takes `pg_control_checkpoint().timeline_id`, or
  assembles the pair from `pg_control_system()` alone.
- *Part B, live and non-forcing.* At the first read after `pg_is_in_recovery()`
  turns false following `pg_promote(wait := false)`, record both the WAL-derived
  timeline and `pg_control_checkpoint().timeline_id`; fail if the WAL-derived value
  is not the promoted timeline. **This arm cannot force the lag** - the measured
  run reached an end-of-recovery checkpoint and read the new timeline immediately.
  Part B detects the lag when it occurs and is **not** evidence about the choice of
  source.

**2.4 `pg_upgrade` is not a terminal denial.** BLOCKED: a second PostgreSQL
major-version binary in the fixture, and the re-domain ceremony (Open 5). A cluster
with a live app, `pg_upgrade`d to the next major; an authority read before the
ceremony, the audited ceremony, a read after. Before: `AUTHORITY_DOMAIN_MISMATCH`;
after: the app serves. **Both halves are required** - the denial half alone is
satisfied by a platform that can never upgrade PostgreSQL, which is the failure
this arm exists to prevent. The contract sentence it depends on: "terminal, with no
re-resolution" is a property of a **binding**; the ceremony mints a new authority
generation and new bindings, it does not re-resolve an existing one. Without that
distinction this arm and the terminal rule contradict each other.

**2.5 The pair is not an authority generation.** BLOCKED: two clusters in one
fixture; the second assertion additionally on the ledger. Two standbys from one
basebackup, promoted in isolation from the same ancestor; the reader against each.
Both report the **same** `(system_identifier, timeline_id)` while holding divergent
data; the ledger's authority generation must differ. **This arm expects
agreement**, and its value is that it pins the property: any later change treating
the pair as a branch identifier fails here.

#### Group 3 - identity. Blocked on the ledger and its projection

Layer 1 and layer 2 must both exist before any arm here runs. `A` and `B` are two
identities of one lifecycle entity; every arm is stated so the entity can change
without the arm changing.

**3.1 Two codes, two moments, one handle.** BLOCKED: ledger, projection. An isolate
holds a handle for identity A; the entity is retired, so the ledger holds A's
tombstone and the projection is written. Then (a) an operation on the stale handle
while retired, then (b) the entity re-provisioned as B and the same handle used
again. Observable: (a) `APP_DEPROVISIONED`, (b) `STALE_APP_INCARNATION`, both
terminal, neither retried. Red when the tombstone is cleared on recreation - which
turns both into successes, because a descriptor-compatible recreated schema is
reachable by the A handle - or when the two codes collapse into one. An arm
asserting only "denies" cannot tell (a) from (b), and the audit trail needs "it is
gone" distinguished from "it came back without you". The distinction is axis-free;
the code names are not, so if the axis moves the codes are renamed and this arm is
untouched.

**3.2 A deprovision does not abort an in-flight operation.** PostgreSQL only. The
no-abort half runs today; the fail-closed half is BLOCKED on the projection. A
deprovision arrives through the lifecycle handle while an operation is in flight;
the in-flight operation returns its own result unaffected (it uses the operator
pool and touches no data-plane pool); the next operation for the same app is denied
at its authority read. Red when deprovision yanks the backend from under live `Rc`
holders. **Scoped to PostgreSQL deliberately:** SC-2's `Command::ReattachFile`
(`crates/zeroship-data-sqlite/src/session.rs:306`) **must** settle or abort
outstanding reservations and detach both connections before restore's file swap, so
an unscoped no-abort rule and SC-2 are jointly unsatisfiable on SQLite. Graceful
deprovision and forced file detach are different events.

**3.3 A durable run created under A does not replay against B.** BLOCKED: the
ledger, plus three record changes that do not exist. A workflow run made **durable
under A, before B exists** - that ordering is the entire arm, because a run created
after the recreate carries B correctly on any implementation, including one that
resolves identity at replay time. Retire, re-provision as B, attempt replay of the A
run: replay is refused and B's pinned isolate is never entered. Red when identity is
resolved at claim or replay time instead of persisted at creation; **a mutation
replacing the persisted token with a fresh lookup must turn this red**, and that
mutation is the only proof, because the two implementations are indistinguishable on
any run created after B.

- **The records that must change first:** `CandidateRun` carries `app_id`,
  `deploy_id` and `deploy_hash` and no lifecycle token
  (`crates/zeroship-plugin-workflow/src/claim.rs:38-52`), its claim query selects
  exactly those (`claim.rs:94`), and the pinned key is
  `PinnedWorkflowKey { app_id, deploy_hash }`
  (`crates/zeroship-worker/src/cache.rs:29-32`).
- **Carrying the identity on the version poll does not discharge this.** The poll
  delivers the *current* identity; once B exists it returns B, and nothing can
  reconstruct that an already-durable journal belongs to A. Everything else here
  fences a handle against the present; this is the one case that must fence it
  against the past.
- *Axis note:* a run is created by an app and replays against a database, so under
  the decoupling the token persisted at creation is the identity of what the run
  BINDS TO, not of the app that started it. The arm is unchanged; the column it needs
  is on the same record either way.

**3.4 The worker's delayed deprovision does not act on the successor.** BLOCKED: the
ledger, and a pending set that can carry a token. An app leaves the version feed and
its id enters the poller's pending set; the entity returns as B before the entry
drains; the cleanup must be skipped rather than applied to B's resources. Red when
the pending entry is a bare id. It is today - the set is `HashSet<Uuid>`
(`crates/zeroship-worker/src/sync.rs:137`, drained in the loop below it) - so there
is nothing to compare and the arm has no subject.

**3.5 A same-id recreation does not serve the previous app's env snapshot.** BLOCKED
on the identity the GC must key on; **buildable today as a failing reproducer**, and
it cannot go green before Fork C. App-axis by nature. An app with a cached env
snapshot; its id leaves the version map and reappears; the poller's GC pass runs,
then an env read for the new app, which must not observe the old snapshot. **This
arm goes red on today's tree**, which is why it is worth keeping:
`e.retain(|app_id, _| versions.contains_key(app_id))`
(`crates/zeroship-worker/src/sync.rs:216`) reclaims by app id alone, so the previous
app's snapshot is retained until an `env_version` bump displaces it. Registered as
L19.

**3.6 A restore does not resurrect a retired entity's bindings.** BLOCKED: the
ledger; rows 1, 2 and 4 additionally on the reconcile procedure. A matrix over the
same four restore shapes as 2.1, and **the expected observable differs by row**.

- *Row 3 (promoting restore).* The domain moved. A binding bound to the pre-restore
  pair denies with `AUTHORITY_DOMAIN_MISMATCH`. Red when the identity is unqualified
  by the domain - the arm must fail against a bare token, or it tests the token and
  not the defence.
- *Rows 1, 2 and 4 (same-domain rewinds).* The entity is **unavailable to workers**
  until the projection is reconciled from the ledger under a newly minted,
  out-of-band authority generation. A worker read denies before reconciliation and
  succeeds after, and the generation minted differs from the pre-restore one. Red
  when the implementation lets a worker proceed on an unchanged pair after a restore,
  which is precisely what the domain alone permits. **No database-resident record on
  either side distinguishes these rows** - restoring either cluster rewinds that
  side's own tombstone - so the ledger is the only witness, and this half cannot be
  softened into something a single cluster can answer.

**3.7 The tombstone survives, bounded.** BLOCKED: the ledger, present in the fixture
in its own recovery domain. The property is: never deleted by any operation the
platform performs, never deleted by a recovery that reaches archive recovery,
**deleted by a restore that bypasses archive recovery** - which is why 3.6's
reconcile exists. A tombstone appended for A, then a recreation appending B which
must not remove A's tombstone; then the four restore rows of 2.1 applied to the
**application cluster**, plus a fifth applying a restore to the **ledger's own**
cluster. Rows 1 to 4 leave the ledger holding A's tombstone and B's head; row 5 is
the one that may lose it, and its expected outcome is that the projection refuses to
serve rather than trusting a rewound ledger. Red when the ledger shares a recovery
domain with the app cluster. **A fixture that restores only the app cluster cannot
see this** - it proves nothing about independence, it assumes it.

**3.8 The head moves by CAS.** BLOCKED: the ledger. A head at A; two concurrent
recreate attempts, both expecting A; exactly one appends B, the other is refused and
appends nothing. Red when the head is a last-writer-wins update, under which both
report success and one identity is lost. The head is per lifecycle entity, so the
decoupling changes how many heads the ledger holds, not what this arm asserts about
one of them.

#### Group 4 - dev. Buildable once the sum type exists

**4.1 An ordinary dev restart does not deny replay.** A dev workflow made durable,
the dev process restarted against the same `.zeroship/dev.sqlite`; the workflow
replays. Red when the attach generation is a per-process random value.

**4.2 An HMR reload does not change the attach generation.** A handle taken, then a
code reload; the handle still resolves. Red when the HMR supervisor generation is
reused as the attach generation.

**4.3 Replacing the app file does change it.** A handle taken, then a detach and
reattach, or the app file replaced; the old handle denies. Red when the generation
is minted once per process. **4.1 and 4.3 must be run as a pair**; either alone is
satisfied by an implementation the other rejects.

**4.4 A dev workflow does not replay against a replaced app database.** BLOCKED on
Open 4, a policy decision rather than code. The journal is a separate database -
`.zeroship/workflows.sqlite` against `.zeroship/dev.sqlite`
(`crates/zeroship-cli/src/main.rs:334-340`) - so deleting or replacing the app file
leaves an old workflow eligible to replay against the new one. Under an atomic reset
policy the observable is an empty journal; under a supervisor-owned project lifecycle
generation it is a denied replay. The two fixtures are different, so the arm cannot
be written first.

---

## Why it is this way

- **A backend slot shared across a thread, and configuration decided once for the
  process, cannot be anchored in something scoped to one thread and minted by
  whoever gets there first.** The plugin set is memoised into a `thread_local!`
  (`PLUGIN_SET`, `cache.rs:228`), so an n-thread worker holds n plugin sets and the
  memo cannot be the process-wide owner. That is why the prototype is a service
  field and `build_runtime` clones an `Arc`.
- **`DbService` must never hold a driver connection.** The `Send + Sync` assertion
  is the enforcement, not a comment: `Pool` is `!Send`. Adding a `Send` wrapper to
  get a pool into the service defeats the check rather than satisfying it.
- **Per-thread resources under `Rc`, process-wide state under `Arc`.** The asymmetry
  is deliberate and load-bearing; see 1.4.
- **Privilege follows the process.** The worker executes creator code, so the
  projection is a thing a separate service writes and the worker only reads. No
  `SECURITY DEFINER`, no session ceremony, no privileged operation the worker can
  call. This is why layer 2 cannot become the authority and why creator-schema
  storage is categorically disqualified - the migrator owns that schema.
- **The incarnation must be domain-qualified, and the domain is not enough.** The
  pair defends promoting recoveries and is blind to non-promoting ones; the standard
  the fence must meet is "a fence that a routine operator recovery can erase is not
  a fence". Only a witness outside every rewind domain meets it absolutely.
- **The comparison is terminal.** No re-resolution on mismatch, and no versioning
  value may be reused to carry it. A value that legally changes under a migration
  either revokes healthy bindings when pinned or follows the recreated app when
  followed.
- **Same-id recurrence is not offered by the creator API, and that does not
  generalise.** App ids are database-minted (`id: t.uuid().notNull().default(uuidV4())`,
  `db/migrations-ts/20260702000200_control_tables.ts:152`), creation supplies no id
  (`INSERT INTO zeroship.apps (name, plan_id) ... RETURNING
  id` in `Registry::create_app`, `crates/zeroship-control/src/registry.rs`; the
  two api-key columns this line used to name are deleted), and retirement is an
  archive that never removes the row. But operator tooling, direct provisioning, test
  fixtures that insert explicit ids, and above all PITR sit outside that path. PITR
  is this proposal's own threat model: a tombstone written into a rewindable table is
  itself rewindable, and a rewound tombstone is no tombstone.
- **A recreated app's schema is descriptor-compatible.** That is why DDL validation,
  schema epochs and descriptor comparison cannot substitute for Fork C: two
  incarnations of one app id have the same shape.
- **Dev is a different type, not a weaker instance of the same one.** A pretend
  incarnation in dev would make the dev tier appear to exercise fences it cannot.

---

## Open

**1. Fork C's layer-1 ledger has no valid home. NEEDS-DECISION.** This contract
requires an append-only lifecycle ledger **in a recovery domain independent of the
application cluster** - a whole-cluster restore must not rewind the record and the
data it fences in the same instant. Decision 17 places database lifecycle on
`zeroship-migrate-server`, and that service reads the *same* PostgreSQL cluster as
the application data and every other platform service: `deploy/compose/docker-compose.yml`
gives migrate-server `postgres://zeroship_control:...@postgres:5432/zeroship` (`:468`)
against control's `:332`, the worker's `:682` and auth's `:825`, all one `postgres`
service (`:72`). Control is disqualified for the same reason and additionally by
decision 17 (control is never in the lifecycle path and holds no provisioning DSN).
Creator-schema storage is disqualified because the migrator owns that schema. **No
document in the set names a home that satisfies the independence requirement.** The
decision needed: either name a second, independently recoverable store for layer 1
(a separate cluster, or an append-only ledger mirrored to the blob store), or
explicitly downgrade property 1 to its bounded form and accept that a snapshot
restore erases the fence. Every group 3 arm is blocked on this.

**2. The ceiling's configuration source and format. NEEDS-DECISION.** "Worker
configuration" names the worker's composition point; `zeroship serve` and the Vite
dev vector are separate composition points that SC-4 does not cover. A dev tier with
no ceiling source, combined with SC-6's "failure is denial", denies every non-`auto`
unmask in dev permanently.

**3. Where the platform master key comes from. NEEDS-DECISION.**
`zeroship-core`'s `PLATFORM_SECRETS` (`crates/zeroship-core/src/config/secrets.rs:88`)
has no row for a database column-key master, and `ZEROSHIP_CONTROL_MASTER_KEY`
(`secrets.rs:95`) is by its name the control plane's. Reusing it puts one secret in
two trust domains - the control plane and the process that executes creator code -
which is the shape "privilege follows the PROCESS" warns about. A new named platform
secret with its own floor is the shape that fits; naming it is not this document's
call.

**4. Dev's atomic reset policy versus a supervisor-owned project lifecycle
generation. NEEDS-DECISION.** Blocks arm 4.4; the two choices need different
fixtures.

**5. The `pg_upgrade` re-domain ceremony. NEEDS-DECISION.** A major-version upgrade
mints a fresh `system_identifier` with no rewind, and the terminal comparison would
permanently deny every app. No document specifies the ceremony. Blocks 2.4.

**6. Which entity the fence keys on: the app, the database, or the grant.
NEEDS-DECISION.** Under the app/database decoupling the fenced entity is the
database. Group 3 is written to survive either answer; 1.4 inverts, and the SQLite
attach path is app-keyed end to end (file name, alias and cache key are all the app
id), so all three must be re-keyed together.

**7. The group 2 recovery fixture. BUILDABLE, 6-10 hours.** A fixture that can
create, crash, snapshot, promote and restore a PostgreSQL cluster, plus the domain
reader itself. Unblocks 2.1, 2.2 and 2.3 - the whole group needs only the reader and
the fixture, not the ledger, the incarnation or a lifecycle service, which is why it
is worth building before anything depends on the domain.

**8. Arm 1.4's cardinality half. BUILDABLE once `DbThreadResources` exists, 2-3
hours.** The type has zero occurrences today, so the arm has no subject. The
discrimination half stays blocked on Open 1.

**9. Arm 3.5 as a failing reproducer. BUILDABLE, 2-3 hours.** It goes red on today's
tree and cannot go green before Fork C. Landing it red-and-ignored, or as a
documented reproducer, records L19 in executable form.

---

## History

Deliberation for this contract lives in
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md` (what is settled, what is
open, what blocks what), `...-runtime-db-binding-decision-log.md` (decision 17, the
lifecycle home) and `...-runtime-db-binding-verification-record.md` (the seven ways a
green run rules on nothing).

Do-not notes, each from something that went wrong:

- **Do not compare `Arc::as_ptr(..) as usize` across a thread boundary.** It reported
  EQUAL under the mutation reintroducing per-thread prototypes, because the plugin
  set is a `thread_local!` dropped at thread exit and the allocator reused the freed
  address. Hold both `Arc`s alive and use `Arc::ptr_eq`.
- **Do not write arm 1.3 with one deletion.** "One pool per deletion" and "one pool
  ever" are the same number at n=1.
- **Do not omit arm 1.3's fourth deletion.** `close_operator_pools` shipped as a
  `#[cfg(test)]` helper, which made "the pool is never closed" true of every shipped
  binary while looking handled in the tests.
- **Do not keep the operator pool installed between batches** to avoid the reconnect.
  Holding two idle backends from the first deletion until process exit is the cost
  that was deliberately refused.
- **Do not assert only "a usable backend is installed" for the singleflight.** Eight
  sequential opens leave one installed too. The count is the assertion.
- **Do not run 2.1 or 3.6 on the promoting restore alone.** That single row is the
  one shape where the domain moves, so it reports a working fence over three shapes
  where the domain contributes nothing.
- **Do not write a deny-only fixture for 2.2.** Without the promoted control it is
  satisfied by a reader that denies everything.
- **Do not take the timeline from `pg_control_checkpoint()` or assemble the pair from
  `pg_control_system()`.** The former lags promotion; the latter carries no timeline
  and would make the reader compare a constant.
- **Do not run 4.1 or 4.3 alone.** Each is satisfied by the implementation the other
  rejects.
- **Do not reuse the HMR supervisor generation as the dev attach generation**, and do
  not mint a per-process random value: dev workflow records persist across restarts,
  so a per-process token denies replay of every dev workflow after an ordinary
  restart.
- **Do not discharge a blocked identity arm with a placeholder.** A bare app id, a
  deploy hash or a lazily minted token is the identity the fence exists to
  distinguish from.
- **Do not cite the same-thread sharing test as evidence of service ownership.** It
  passes on code that predates this contract, because `THREAD_DB_CTX` is
  thread-local either way.
- **Do not read a built classifier as a live fence.** The authority machine's
  consuming side is complete and tested while its production input is a constant;
  see "The consuming side is built" under Fork C. `AppIncarnationId`,
  `DbThreadResources` and `attach_generation` occur zero times in `crates/`, so
  every arm whose subject is one of them has no subject yet.

**Flagged, kept rather than cut** because it could not be sorted cleanly into
protective or narrative: the operator-lifecycle handle exists because
deprovisioning used to be a free function that took a `&str` URL, re-ran backend
selection on it and built a fresh two-connection pool **per deleted app**. That
shape is what arm 1.3's parse and pool assertions exist to keep out. The
`crates/zeroship-plugin-db/src/lib.rs` comment that recorded it is gone.
