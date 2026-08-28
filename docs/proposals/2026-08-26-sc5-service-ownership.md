# SC-5: service ownership

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** merge 5a of that document (the behaviour-neutral half of the cutover).

*(This also read "and the process-wide live-metadata cache, which has nowhere to
live until something owns it". **RETRACTED 2026-08-27 by decisions 7 and 8**:
with the descriptor as sole schema authority and no data-plane introspection,
`live_metadata.rs` has no remaining caller, so the cache has nothing to hold
rather than nowhere to live. Step 5a shipped that module and today's review
closed four defects in it; the sequencing consequence is recorded in the parent's
section 9 and no deletion order is proposed here.)*

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## Why this exists

The parent proposal describes an isolate-owned `DbIsolateBinding` holding
`Rc<DbThreadResources>`, and a **process-wide** live-metadata cache. Neither has
an owner today, and two concrete facts show it:

- **The plugin set is memoised per THREAD, which is not process-wide.**
  `plugin_set()` mints the vector on first use and "returns clones of the same
  `Arc`s on every call, so two runtimes built on one thread share plugin
  instances rather than each holding their own"
  (`crates/zeroship-worker/src/cache.rs:190-202`), and `build_runtime` calls it
  (`cache.rs:426`). But the slot it memoises into is a `thread_local!`
  (`PLUGIN_SET`, `cache.rs:186-188`), so an n-thread worker still holds n plugin
  sets and there is still no process-wide owner for a process-wide cache.

  **This bullet said the opposite until it was re-derived against the tree on
  2026-08-27**, and the correction matters because this is one of the two facts
  the document exists on. It read "`create_plugins()` is called inside runtime
  construction (`cache.rs:387`), so every `build_runtime` mints a fresh plugin
  set rather than cloning one prototype." That was true when written and is not
  true now - `cache.rs:938` records in the tree's own words that `build_runtime`
  "used to call `create_plugins()` on every build" - and `cache.rs:387` is now
  unrelated cache code. The per-runtime mint is fixed; the per-thread scope is
  what remains, and it is the part this contract is actually about.
- **Operator deprovisioning opens its own database.** `deprovision_app_cdc`
  takes a `db_url`, re-runs `backend_for_url`, and calls `Pool::connect` for
  itself (`crates/zeroship-plugin-db/src/lib.rs:901,904`) - a second backend
  selection and a second pool, parallel to whatever the worker already has.

A cache that must be shared across **isolates on every thread**, and a backend
slot that must be shared across a thread, cannot be anchored in something scoped
to one thread and minted by whoever gets there first.

## The contract

One `Arc<DbService>`, constructed once at worker/CLI composition, before any
isolate exists. It owns:

| Owned | Why it cannot live per-runtime |
| --- | --- |
| Validated configuration (`DbPluginConfig`) | Backend selection must happen once, not per isolate; the parent proposal forbids a second URL parse |
| The plugin prototype | So `build_runtime` **clones an `Arc`** instead of minting a plugin set - today that clone comes from a per-thread memo (`cache.rs:190-202`), not from a process-wide owner |
| The stable thread-resource key | So current and deploy-pinned isolates on one OS thread resolve the *same* `DbThreadResources` |
| ~~The process-wide live-metadata cache~~ **NO SUBJECT, 2026-08-27** | ~~Its values are immutable plain data; sharing is the point, and a per-runtime owner cannot provide it~~ Decisions 7 and 8 remove per-operation introspection, so `live_metadata.rs` has no remaining caller and there is nothing for this service to own |
| The neutral operator-lifecycle handle | So deprovision uses the service's backend rather than reparsing the URL and opening a second pool (`lib.rs:901,904`) |
| **The operator mask ceiling** (added 2026-08-27, decision 4) | It is operator configuration, immutable per isolate, and every binding meets it with that deploy's creator draft - so it must exist before any binding does |
| **The platform master key** (added 2026-08-27, decision 1) | Column keys are derived `HKDF(platform_master_key, app_id, key_version)`, so the key is service-level configuration rather than a per-app lookup |

**Both additions are consequences, and both leave a question this contract
cannot answer on its own.** They are listed here because "constructed once at
worker/CLI composition, before any isolate exists" is exactly the lifetime each
needs, and because a value that arrives any later re-opens the staleness
problems decisions 1 and 4 exist to remove.

What is **not** specified anywhere in this set:

- **the ceiling's configuration source and format.** "Worker configuration"
  names the worker's composition point; `zeroship serve` and the Vite dev vector
  are separate composition points, and SC-4 does not cover them. A dev tier with
  no ceiling source, combined with SC-6's "failure is denial", denies every
  non-`auto` unmask in dev permanently.
- **where the platform master key comes from for this crate.**
  `zeroship-core`'s `PLATFORM_SECRETS` table
  (`crates/zeroship-core/src/config/secrets.rs:88-129`) has no row for a
  database column-key master, and `ZEROSHIP_CONTROL_MASTER_KEY` is by its name
  the control plane's. Reusing it puts one secret in two trust domains - the
  control plane and the process that executes creator code - which is the shape
  `AGENTS.md`'s "privilege follows the PROCESS" invariant warns about. A new
  named platform secret with its own floor is the shape that fits; naming it is
  not this document's call.

`DbService` is `Send + Sync` because it crosses worker-thread boundaries, but it
holds only validated configuration and `Sync` data - never a driver connection.
`factory.open()` still runs on the owning worker thread and yields an
`Rc<dyn DbBackend>`; nothing becomes `Send` merely to satisfy service storage.

**One requirement this contract states about code it does not own: the GC
identity of a per-app cache must include the incarnation.** The worker's env
cache is reclaimed by app id alone, so a same-id recreation retains the previous
app's env snapshot and serves it to the new one until an `env_version` bump
displaces it. That is a stale-secret leak rather than an untidy cache, and it is
the same identity gap Fork C closes for DB handles - a fence on the DB door
while the env door stays open is a fence around the wrong door. The defect
itself belongs to another subsystem and is recorded in the defect register (the
worker env cache entry); what belongs here is that the incarnation this contract
mints is the identity that GC must key on.

## Lifecycle, including the case the parent proposal did not state

Round 3 asked for one more axis than "share one slot and one cache": **what a
deprovision arriving while an isolate still holds a handle does.**

- A deprovision is routed through the service's lifecycle handle, so it uses the
  same backend the data plane uses - not a second pool.
- It **does not** invalidate handles already cloned into live isolates. Those
  hold `Rc`s and may have futures in flight; yanking the backend underneath them
  would turn an operator action into a data-plane crash.
- Instead it marks the app **deprovisioned in the service**, so no *new*
  operation resolves resources for it, and existing operations fail closed at
  their next authority read - which finds the **tombstone** and denies
  terminally.

  Not "which by then has no row". An earlier draft said exactly that, and it
  contradicts the tombstone rule below: a deleted row is not a tombstone, and an
  implementer following the deleted-row version would remove the only durable
  evidence that this app id was ever deprovisioned - which is precisely how a
  PITR rewind or a recreated id reopens the cross-incarnation hole. The row
  stays; its state changes.
- The thread-resource entry is dropped when its last holder releases it, which
  is the existing `Rc` semantics rather than a new mechanism.

### Fork C: the durable AppIncarnationId

**HOMELESS as of 2026-08-27, decision 7. The requirement stands; its storage is
deleted.** Everything below specifies `incarnation`, `deprovisioned_at` and the
tombstone rule as columns of `__zeroship_admin.app_schema_state`, and the
authority domain `(system_identifier, timeline_id)` as a property read from the
cluster answering that read. Decision 7 deletes the whole schema.

**These are not schema metadata**, which is why the descriptor cannot absorb
them and why the deferred DDL-validation feature would not cover them either:
two incarnations of one app id have the **same** schema, so any comparison of
catalog against descriptor passes for both. What Fork C fences is identity, not
shape.

**Flagged, not answered** - the parent's section 8 carries the open question in
full. The control plane is the obvious candidate since app lifecycle already
lives there, but it is not decided, and any answer must preserve the four
properties this section spends its length establishing: a durable tombstone that
is never deleted, a CAS against an expected incarnation, an authority domain a
restored dump cannot assert about itself, and three distinguishable error codes.

**Read what follows as the requirement, not the mechanism.** The tombstone
argument, the recreate-clause correction and the shape are all still the
specification of *what must hold*; only the storage they name is gone.

This is the fork the parent proposal names `Fork C` ("what fences a stale
handle") and defines by pointing back here. It is stated as a decision, not only
as the correction that produced it - the correction follows immediately, because
an implementer who does not know which clause was wrong will write it again.

#### The tombstone is permanent, and an earlier draft's recreate clause was the bug

An earlier draft said "an app can be deprovisioned and recreated with the same
id, and the recreated app must not inherit a stale marker". That clause created
a real hole: a marker keyed on `app_id` alone, **cleared on recreation**,
reauthorizes a handle cloned before the deprovision - and if the recreated
schema is descriptor-compatible, that handle reads the new epoch and reaches the
**new** app's data. The binding identity carries `app_id`, `deploy_hash` and
`runtime_instance_id`, no incarnation, so nothing else distinguishes them.

The three round-6 reviewers split two-to-one: mint a durable incarnation, or
delete the requirement on the ground that same-id recreation is not an operation
the platform offers. **The incarnation is adopted.** The dissent's premise is
true about the ordinary creation path and does not generalise:

- app ids really are database-minted on that path. `zeroship.apps.id` is
  `t.uuid().notNull().default(uuidV4())`
  (`db/migrations-ts/20260702000200_control_tables.ts:106-108`) and the creation
  path inserts `(name, plan_id, api_key, api_key_hash)` `RETURNING id`, supplying
  no id (`crates/zeroship-control/src/registry.rs:245-251`); deletion is a hard
  `DELETE FROM zeroship.apps WHERE id = $1` (`registry.rs:369`) with no
  soft-delete column and no restore path in the registry.
- **But "the ordinary creator API does not supply an id" is not "an id can never
  recur."** Operator tooling, direct provisioning, test fixtures (which already
  insert explicit ids) and above all **PITR** sit outside that path.

PITR is what settles it, and it is this proposal's own threat model rather than
an imported one. The parent already binds cache authority to
**`(system_identifier, timeline_id)`** rather than `system_identifier` alone,
because same-cluster PITR preserves the latter and `timeline_id` "is what moves
when recovery rewinds and promotes"; it also records that a dump rewinds the
per-app platform tables. So a tombstone written into `app_schema_state` is
**rewindable**, and a rewound tombstone is no tombstone at all - the
deprovisioned app's authority row returns, with no creator API having reused
anything. A fence that a routine operator recovery can erase is not a fence.

#### The shape, therefore

1. An opaque 128-bit `AppIncarnationId`, minted **by the privileged server-side
   function, never chosen by a caller** - the same rule the epoch already
   follows, for the same reason. It is persisted beside `state` and `epoch`,
   carried in every binding, and compared before any data SQL.
2. **It is qualified by the authority domain `(system_identifier, timeline_id)`,
   or PITR simply resurrects an old token along with the row that holds it.**
   The incarnation inherits the parent's PITR defence rather than re-opening the
   hole one field over. This is the correction that makes the token worth adding
   at all.
3. **A tombstone is never cleared.** Deprovision leaves it; recreation mints a
   new incarnation rather than erasing the old marker.
4. **An epoch mismatch and an incarnation mismatch mean different things, and one
   value cannot carry both.** An epoch mismatch means *re-resolve*: the schema
   moved, pick up the new one - the parent explicitly permits a binding to
   observe a new epoch and resolve again, and restore publishes a fresh epoch for
   the same logical app. An incarnation mismatch means *deny, terminally*. Using
   the epoch as the handle fence has only two outcomes and both are wrong: pin it
   and every migration revokes healthy bindings, or follow it and an old handle
   follows the recreated app exactly as it does today.

It costs no hot-path round trip: the authority batch already reads this row
before `SET LOCAL ROLE`, so the incarnation is read with the epoch it sits
beside.

## Acceptance shape

### The sharing arm was already satisfied

**It passes on today's code, so it cannot discriminate this contract's work.**
`DbPlugin` carries `url`, `worker_id` and `meter` - **no backend and no pool**
(`crates/zeroship-plugin-db/src/lib.rs:301-311`); the pool and backend live in
the thread-local context, so two isolates on one OS thread have always shared
them. Minting a fresh `DbPlugin` per `build_runtime` never produced two
backends.

**The full treatment lives in the verification record**, under "A fifth: the arm
that already passed before the work began" - including the commit message that
claimed this change made "two isolates share a backend" when what it actually
buys is avoiding a per-build re-mint of the plugin vector, and the three stale
citations found when that material was re-derived.

It is stated there rather than here because it is a **mechanism** - a class of
arm that reports green without ruling on anything - and mechanisms belong in the
document that catalogues them, where the next author looks. What belongs in this
contract is only the consequence for this contract: **this arm needs a
discriminating partner before it is cited as evidence for service ownership.**

*(An earlier revision carried the full text in both places while the record
claimed the material had been "moved". It had been copied. Two copies of a
finding drift, and the version that goes stale is the one nobody is reading.)*

### Every arm below needs its inverse checked

**An arm that already passes on today's code proves nothing about the change.**
Two arms in an earlier draft of this list did exactly that, and the reason is
worth stating because it will catch the next author as well: the DB context is a
`thread_local!` - `THREAD_DB_CTX`
(`crates/zeroship-plugin-db/src/context.rs:977`). It is per-**thread**. So two
isolates on one thread *already* share one context, and an arm asserting they
share one slot passes before the work is done.

**The naming trap that produced those two arms has since been removed, and that
is recorded rather than quietly edited**, because the arms it produced are still
in this list and a reader needs to know why they were written. The symbol was
`ISOLATE_CTX`, its type was `IsolateDbContext`, and its doc comment called it
"the per-isolate DB context" - so a reader could believe two isolates held two
contexts. The tree now says the opposite in both places: the thread-local's own
comment reads "The DB context shared by all isolates on this worker thread"
(`context.rs:973`), and the type is `ThreadDbContext`, "deliberately named for
its actual ownership: a worker thread may host many app isolates ... and all of
them share this value" (`context.rs:122-126`). The name changed; the mechanism
did not, and neither did the trap for anyone writing an arm from memory. This
citation has now been re-derived twice - once when the declaration moved with
the frame-effect work, once when the rename landed - which is itself the
argument for citing a symbol rather than a line.

### The arms

- **Two OS worker threads** resolving the same
  `(authority_domain, app_id, incarnation, epoch)` concurrently converge on
  **one** process-wide cache entry - asserted by `Arc::ptr_eq` on the facts each
  thread resolves, not by counting - while each thread keeps its **own** driver
  resources.

  **The catalog-walk count for this arm is "at most `n_threads`", NOT one**, and
  the distinction is the whole reason to state it. An earlier draft asked for
  "one process-wide cache/catalog resolution", which silently conflated two
  different things and made the arm **unsatisfiable against this design**: the
  parent chooses a **per-thread** singleflight and lists process-wide
  singleflight under rejected alternatives, precisely because it "needs a
  cross-thread wake path unverified here, to save at most `n_threads` catalog
  walks per epoch bump". So two threads racing a cold miss *will* both walk the
  catalog. What they must not do is end up with two cache entries or two
  distinct fact objects. Writing the arm the old way meant a correct
  implementation failed it and the only way to pass was to build the mechanism
  the parent had rejected - an acceptance criterion that legislates against its
  own design.

  `Arc::ptr_eq` is the right instrument here because the cached facts are
  process-wide and behind `Arc`. It is **not** the right instrument for the
  same-thread arm below; see the note there.

  **Each thread must RESOLVE its own handle. Handing both threads one cloned
  `Arc` is the obvious way to write this arm and it cannot fail** - reported by
  the implementation of 5a, 2026-08-27, and confirmed against its mutation
  evidence. Cloning one `Arc<LiveMetadataCache>` into two threads asserts that a
  clone of a pointer equals itself, which is true on a **per-thread**
  implementation too. The arm has to make each thread reach for the cache the
  way production code does - through its own context - and only then compare.
  Same disease as the criterion corrected above, one level down: the earlier one
  legislated against the design, this one legislates nothing at all.

  **A second trap, found by the 5a implementation's own mutation run and worth
  repeating because it is invisible in review.** The natural way to compare
  across threads is to return `Arc::as_ptr(..) as usize` from each and compare
  the integers in the parent. **Under the mutation that reintroduces per-thread
  prototypes, that comparison reported EQUAL.** A thread's plugin set is a
  `thread_local!` and is dropped at thread exit, so the allocator handed the
  second thread the address the first had just freed: two freshly minted
  objects, identical addresses. The arm must return the `Arc`s themselves and
  keep both alive in the parent while comparing, which makes address reuse
  impossible. An arm that compares raw addresses across a thread boundary is
  measuring the allocator, not the design.

  Two threads, not two isolates on one thread, and that is the whole point of
  the arm. The contract being tested is that the live cache is *process-wide*;
  a same-thread arm passes identically on a wrong implementation that keeps one
  service and one cache **per thread**, because `THREAD_DB_CTX` is thread-local
  (`crates/zeroship-plugin-db/src/context.rs:977`) and both isolates share it
  either way. The same-thread arm below is still worth keeping - it catches
  duplication *within* a thread - but it cannot see the property this bullet
  exists for.
- Current and deploy-pinned isolates for one app on one worker thread resolve
  **one** `DbThreadResources` and **one** cache entry for the same
  `(authority_domain, app_id, incarnation, epoch)`, asserted by **`Rc::ptr_eq`**
  on the resources plus `Arc::ptr_eq` on the cached facts, with backend factory
  opens and catalog introspections counted **separately**.

  **`Rc`, not `Arc`** - this arm said `Arc::ptr_eq` and would not have compiled.
  `DbThreadResources` is held as `Rc<DbThreadResources>` (parent, the
  `AppDbBinding` struct) and "The contract" above states that driver resources
  stay non-`Send` `Rc` deliberately, because they never cross a thread. The two
  pointer checks in this arm are therefore over **different**
  smart pointers, and that asymmetry is the design: per-thread resources under
  `Rc`, process-wide facts under `Arc`. An arm that used `Arc` for both would
  have been a compile error dressed as a cross-thread guarantee.

  Not by counting connections. A connection count cannot prove map cardinality -
  it is the wrong instrument for the claim - and it is not even the parent's
  cache model, which keys entries by `(app_id, epoch)` and lets old generations
  survive until eviction. The arm must also be shown to **fail** on a
  deliberately duplicated resolution, or it is measuring nothing.
- `build_runtime` performs **no** backend selection and opens **no** pool; it
  clones an `Arc` from the service.
- Operator deprovisioning performs **no** second URL parse, and opens **at most
  one** operator pool for the life of the process.

  **This clause said "opens no second pool" until 2026-08-27 and was not
  satisfiable as written** - found by the adversarial review of the 5a
  implementation, which correctly refused to let the implementation quietly
  reword it. Deprovision runs on the version-poller thread, which has no
  `ThreadDbContext` and therefore no data-plane pool to reuse; "no second pool"
  can only be met by not connecting at all. What the clause was actually
  protecting against is a pool **per deletion** - the pre-5a shape, paying two
  connects, two authentications and two TLS handshakes for every deprovisioned
  app. One memoised pool delivers that.

  The cost the corrected wording makes visible, and which the old wording hid:
  a worker would otherwise hold those connections from its first deletion
  **until process exit**, against no configured budget.

  **The implementation closed that rather than only disclosing it**
  (2026-08-27). `close_operator_pools` uninstalls the pool and is called by the
  version poller (`zeroship-worker/src/sync.rs`) once its reconcile pass is
  done, so the correct clause is **at most one operator pool installed at a
  time, released when the poller's work completes** - not one held forever. Two
  details are load-bearing and belong here rather than only in the code:

  - The release must be **called where a runtime can still drive shutdown**.
    Dropping a `Pool` asks its detached driver tasks to close; it does not wait
    for them.
  - `close_operator_pools` began as a `#[cfg(test)]` helper, "which is what made
    *the pool is never closed* true of every shipped binary while looking
    handled in the tests". That is the same shape as L12b, where the
    abandoned-slot reaper existed but was reachable only from tenant JS. A
    release path with no production caller is a capability, not a release path,
    and the acceptance arm must therefore assert that a deletion **after** the
    release opens a second pool - if the count stays at one, the release did
    nothing.

  The general lesson, and the third instance in this document set: an
  acceptance criterion stated as an absolute (`no`, `never`, `exactly one`)
  is a claim about what the implementation can reach, and it is worth asking
  *on which thread, holding what* before writing it down.
- **PostgreSQL arm:** a deprovision arriving while an isolate holds a handle does
  not abort an in-flight operation; the next operation for that app fails closed.

  Scoped to PostgreSQL deliberately. SC-2's `DetachApp` **must** settle or abort
  outstanding reservations and close both connections before restore's file swap
  (sc2, "The reservation protocol", and the arm that repeats it in sc2's
  acceptance shape), or a connection is left bound to an obsolete inode - so an
  unscoped no-abort rule and SC-2 are jointly unsatisfiable on SQLite. Graceful
  app deprovision and forced file detach are different events and only the first
  is covered here.
- A handle cloned before a deprovision is denied **terminally** at `prepare` on
  incarnation mismatch, and the denial is distinguished from an epoch mismatch,
  which re-resolves. The tombstone is never cleared.
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
  **worker's delayed deprovision** (its pending set stores bare UUIDs) and
  **durable workflow replay** (journal schemas deliberately survive app
  deletion, and pinned-isolate keys are `(app_id, deploy_hash)`). A cleanup or
  replay carrying incarnation A must not act on incarnation B.

  **For durable work the incarnation must be PERSISTED AT CREATION, not looked
  up at replay - and carrying it only on the version poll does not achieve
  that.** The poll delivers the *current* incarnation; once B exists it returns
  B, and nothing can reconstruct that an already-durable journal belongs to A.
  The claim record has no room for it today: the struct carries `app_id`,
  `deploy_id` and `deploy_hash`
  (`crates/zeroship-plugin-workflow/src/claim.rs:40-43`), its claim query
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
  snapshot.** The worker's env cache is reclaimed by app id alone -
  `e.retain(|app_id, _| versions.contains_key(app_id))`
  (`crates/zeroship-worker/src/sync.rs:189`) keeps an entry whenever the id is
  still present in the version map - so today a recreated app **inherits the
  previous app's env snapshot** until an `env_version` bump displaces it. That
  is a stale-secret leak, not an untidy cache. The arm is stated here because
  the incarnation this contract mints is the identity the GC must key on; the
  defect itself is another subsystem's and is recorded in the defect register
  (the worker env cache entry), which also carries the separate retention
  finding on the same code.

  Kept in this list despite being another subsystem's code, because it is the
  same identity gap and would otherwise be found by whoever first recreates an
  app id in anger. The DB fence closing while the env cache stays open would be
  a fence around the wrong door.
- Two isolates racing their first DB operation on one thread enter one
  initialization singleflight and publish exactly one backend - **asserted with
  a test that fails when the singleflight is removed.** Today's per-thread
  `THREAD_DB_CTX` makes the naive form of this arm pass without any singleflight
  at all.
