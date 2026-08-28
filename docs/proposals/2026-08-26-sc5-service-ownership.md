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
handle") and defines by pointing back here. **Its storage is open** - the
identity state lived in a platform-schema row and that schema is deleted; the
parent's section 6 carries the open question, and the control plane is the
obvious candidate only because app lifecycle already lives there. What follows
is the requirement, not the mechanism.

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
assert about itself, and three distinguishable error codes.

## Acceptance shape

**Roughly half the arms below cannot be implemented as written.** They key on
`(authority_domain, app_id, incarnation, epoch)`, and of those four only
`app_id` exists today: the schema epoch is deleted from the design outright, and
the incarnation and the authority domain are specified but have no storage while
Fork C is unhomed. They are stated in the shape they must take once Fork C has a
home; the epoch component drops out with the epoch. An arm that names the
identity is blocked on the identity, and no placeholder - a bare app id, a
deploy hash, a lazily minted token - satisfies it, because each of those is
exactly the identity the fence exists to distinguish from.

### The sharing arm was already satisfied

**It passes on code that predates this contract, so it cannot discriminate this
contract's work.** The pool and the backend live in the thread-local DB context,
so two isolates on one OS thread have always shared them; minting a fresh
`DbPlugin` per `build_runtime` never produced two backends. What the 5a change
actually buys is avoiding a per-build re-mint of the plugin vector and its
`Arc`s. The full treatment - including the commit that claimed more than that,
and the stale citations found when the material was re-derived - is in the
verification record under "A fifth: the arm that already passed before the work
began", because it is a *mechanism* rather than a service-ownership decision.
The consequence for this contract is only this: **that arm needs a
discriminating partner before it is cited as evidence for service ownership.**

### Every arm below needs its inverse checked

**An arm that already passes on today's code proves nothing about the change.**
The DB context is a `thread_local!` - `THREAD_DB_CTX`,
`crates/zeroship-plugin-db/src/context.rs:935`, whose own comment reads "The DB
context shared by all isolates on this worker thread" (`:931`) and whose type is
`ThreadDbContext` (`:141`). It is per-**thread**, so two isolates on one thread
*already* share one context and an arm asserting they share one slot passes
before the work is done. Cite the symbol rather than the line: this declaration
has moved twice in two weeks.

The general rule, worth applying before writing any arm here: an acceptance
criterion stated as an absolute (`no`, `never`, `exactly one`) is a claim about
what the implementation can reach, so ask *on which thread, holding what*.

### The arms

- **Two OS worker threads served by one service hold ONE plugin prototype**,
  asserted by `Arc::ptr_eq`, while each thread keeps its **own** driver
  resources.

  Two traps, both found by 5a's mutation run and both invisible in review:

  - **Each thread must RESOLVE its own handle.** Handing both threads one cloned
    `Arc` and comparing it with itself cannot fail - it passes on a per-thread
    implementation too. The arm has to make each thread reach for the object the
    way production code does, through its own context, and only then compare.
  - **Compare the `Arc`s, not their addresses.** Returning
    `Arc::as_ptr(..) as usize` from each thread and comparing the integers
    reported EQUAL under the mutation that reintroduces per-thread prototypes: a
    thread's plugin set is a `thread_local!` dropped at thread exit, so the
    allocator handed the second thread the address the first had just freed. The
    parent must hold both `Arc`s alive while comparing, which makes address
    reuse impossible. An arm that compares raw addresses across a thread
    boundary is measuring the allocator, not the design.

  Two threads, not two isolates on one thread, and that is the whole point: a
  same-thread arm passes identically on a wrong implementation that keeps one
  service per thread, because `THREAD_DB_CTX` is thread-local and both isolates
  share it either way. The same-thread arm below still catches duplication
  *within* a thread, but it cannot see this property.
- Current and deploy-pinned isolates for one app on one worker thread resolve
  **one** `DbThreadResources` for the same
  `(authority_domain, app_id, incarnation)`, asserted by **`Rc::ptr_eq`**, with
  backend factory opens counted separately.

  **`Rc`, not `Arc`.** `DbThreadResources` is held as `Rc<DbThreadResources>`
  (parent, the `AppDbBinding` struct) and driver resources stay non-`Send` `Rc`
  deliberately, because they never cross a thread. Pointer checks in this
  contract are therefore over two different smart pointers, and the asymmetry is
  the design: per-thread resources under `Rc`, process-wide service state under
  `Arc`. An arm using `Arc` for both is a compile error dressed as a
  cross-thread guarantee.

  Not by counting connections: a connection count cannot prove map cardinality.
  The arm must also be shown to **fail** on a deliberately duplicated
  resolution, or it is measuring nothing.
- `build_runtime` performs **no** backend selection and opens **no** pool; it
  clones an `Arc` from the service.
- Operator deprovisioning performs **no** second URL parse, and holds **at most
  one** operator pool at a time, released when the version poller's pending set
  drains.

  "No second pool" is not satisfiable and must not be written: deprovision runs
  on the version-poller thread, which hosts no isolate and therefore has no
  data-plane pool to reuse, so "no pool" can only be met by not connecting at
  all. What the clause protects against is a pool **per deletion** - two
  connects, two authentications and two TLS handshakes for every deprovisioned
  app. One memoised pool per reconcile batch delivers that
  (`OPERATOR_POOL_SIZE = 2`, `crates/zeroship-plugin-db/src/service.rs:77`).

  Two details are load-bearing and belong here rather than only in the code:

  - The release (`close_operator_pools`, `service.rs:451`) must be **called
    where a runtime can still drive shutdown**. Dropping a `Pool` asks its
    detached driver tasks to close; it does not wait for them. The version
    poller calls it inside its own poll loop
    (`crates/zeroship-worker/src/sync.rs:205`).
  - `close_operator_pools` began as a `#[cfg(test)]` helper, "which is what made
    *the pool is never closed* true of every shipped binary while looking
    handled in the tests" - the same shape as L12b, where the abandoned-slot
    reaper existed but was reachable only from tenant JS. A release path with no
    production caller is a capability, not a release path. **The arm must
    therefore assert that a deletion arriving after the release opens a second
    pool**: if the count stays at one, the release did nothing.

  **Accepted cost, deliberately taken:** a later batch of deletions pays a fresh
  connect rather than the process holding two idle backends from its first
  deletion until exit. Do not "fix" this by keeping the pool installed.
- **PostgreSQL arm:** a deprovision arriving while an isolate holds a handle does
  not abort an in-flight operation; the next operation for that app fails closed.

  Scoped to PostgreSQL deliberately. SC-2's `ReattachFile` **must** settle or
  abort outstanding reservations and detach both connections before restore's
  file swap, or a connection is left bound to an obsolete inode - so an unscoped
  no-abort rule and SC-2 are jointly unsatisfiable on SQLite. Graceful app
  deprovision and forced file detach are different events and only the first is
  covered here.
- A handle cloned before a deprovision is denied **terminally** at `prepare` on
  incarnation mismatch, distinguishably from the tombstone case. The tombstone is
  never cleared.
- **A durable workflow run created under incarnation A does not replay against
  incarnation B**, asserted by creating the run, deprovisioning, re-provisioning,
  and then attempting replay. The arm must exercise a run that was **already
  durable before B existed** - a run created after the recreate carries B
  correctly on any implementation, including one that resolves the incarnation
  at replay time instead of persisting it, so a same-incarnation fixture cannot
  tell the two apart.
- **A PITR rewind that restores a deprovisioned app's authority row does not
  resurrect its bindings**, because the authority domain
  `(system_identifier, timeline_id)` moved. This arm is the reason the
  incarnation exists in its qualified form rather than as a bare token, and it
  must be shown to **fail** against an unqualified incarnation - otherwise it is
  testing the token and not the defence.
- Two known longer-lived old handles are covered explicitly, because both
  outlive the app record and neither carries a lifecycle token today: the
  **worker's delayed deprovision** (its pending set stores bare UUIDs,
  `crates/zeroship-worker/src/sync.rs:137-166`) and **durable workflow replay**
  (journal schemas deliberately survive app deletion, and pinned-isolate keys
  are `(app_id, deploy_hash)`). A cleanup or replay carrying incarnation A must
  not act on incarnation B.

  **For durable work the incarnation must be PERSISTED AT CREATION, not looked
  up at replay - and carrying it only on the version poll does not achieve
  that.** The poll delivers the *current* incarnation; once B exists it returns
  B, and nothing can reconstruct that an already-durable journal belongs to A.
  The claim record has no room for it today: `CandidateRun` carries `app_id`,
  `deploy_id` and `deploy_hash`
  (`crates/zeroship-plugin-workflow/src/claim.rs:38-50`), its claim query
  selects exactly those (`claim.rs:84`), and the pinned key is
  `PinnedWorkflowKey { app_id, deploy_hash }`
  (`crates/zeroship-worker/src/cache.rs:29-32`).

  So the token is written into the run/dispatch record when the durable work is
  created, carried through claim and replay, included in the pinned-isolate key,
  and compared before the runtime loads. This is easy to miss precisely because
  the version wire and the database CAS look end-to-end once both exist -
  **neither can manufacture historical identity for work that is already
  durable.** Everything else in this contract fences a handle against the
  present; this is the one case that must fence it against the past.
- **A same-id recreation does not serve the previous incarnation's env
  snapshot** (`sync.rs:216`, and L19 in the defect register). Kept in this list
  despite being another subsystem's code, because it is the same identity gap
  and would otherwise be found by whoever first recreates an app id in anger.
- Two isolates racing their first DB operation on one thread enter one
  initialization singleflight and publish exactly one backend - **asserted with
  a test that fails when the singleflight is removed.** Today's per-thread
  `THREAD_DB_CTX` makes the naive form of this arm pass without any singleflight
  at all.
