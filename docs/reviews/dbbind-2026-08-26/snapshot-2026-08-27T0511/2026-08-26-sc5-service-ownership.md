# SC-5: service ownership

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** merge 5a of that document (the behaviour-neutral half of the cutover),
and the process-wide live-metadata cache, which has nowhere to live until
something owns it.

---

## Why this exists

The parent proposal describes an isolate-owned `DbIsolateBinding` holding
`Rc<DbThreadResources>`, and a **process-wide** live-metadata cache. Neither has
an owner today, and two concrete facts show it:

- **Plugins are constructed per runtime.** `create_plugins()` is called inside
  runtime construction (`crates/zeroship-worker/src/cache.rs:387`), so every
  `build_runtime` mints a fresh plugin set rather than cloning one prototype.
- **Operator deprovisioning opens its own database.** `deprovision_app_cdc`
  takes a `db_url`, re-runs `backend_for_url`, and calls `Pool::connect` for
  itself (`crates/zeroship-plugin-db/src/lib.rs:909,916`) - a second backend
  selection and a second pool, parallel to whatever the worker already has.

A cache that must be shared across isolates, and a backend slot that must be
shared across a thread, cannot be anchored in something minted per runtime.

## The contract

One `Arc<DbService>`, constructed once at worker/CLI composition, before any
isolate exists. It owns:

| Owned | Why it cannot live per-runtime |
| --- | --- |
| Validated configuration (`DbPluginConfig`) | Backend selection must happen once, not per isolate; the parent proposal forbids a second URL parse |
| The plugin prototype | So `build_runtime` **clones an `Arc`** instead of minting a plugin set (`cache.rs:387` today) |
| The stable thread-resource key | So current and deploy-pinned isolates on one OS thread resolve the *same* `DbThreadResources` |
| The process-wide live-metadata cache | Its values are immutable plain data; sharing is the point, and a per-runtime owner cannot provide it |
| The neutral operator-lifecycle handle | So deprovision uses the service's backend rather than reparsing the URL and opening a second pool (`lib.rs:909,916`) |

`DbService` is `Send + Sync` because it crosses worker-thread boundaries, but it
holds only validated configuration and `Sync` data - never a driver connection.
`factory.open()` still runs on the owning worker thread and yields an
`Rc<dyn DbBackend>`; nothing becomes `Send` merely to satisfy service storage.

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

### The tombstone is permanent, and an earlier draft's recreate clause was the bug

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

The shape, therefore:

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

**The sharing arm was already satisfied, and an implementation claiming to fix
it has now been written and had to be corrected.** `DbPlugin` carries `url`,
`worker_id` and `meter` - **no backend and no pool**
(`crates/zeroship-plugin-db/src/lib.rs:311-320`). The pool and backend live in
the thread-local `ISOLATE_CTX`, so two isolates on one OS thread have always
shared them. Minting a fresh `DbPlugin` per `build_runtime` never produced two
backends.

A commit on the implementation branch nonetheless described itself as making
"two isolates on one thread share a backend instead of each resolving their
own". That is false. What the change actually buys is avoiding the re-mint of
the plugin vector and its `Arc`s - and one S3 client construction - per isolate
build. Worth having, an order of magnitude less than claimed, and a reminder
that this arm reads as if it is about backends when the code makes it about
allocations.

**Every arm below must be checked for the inverse failure too: an arm that
already passes on today's code proves nothing about the change.** Two arms in an
earlier draft of this list did exactly that, and the reason is worth stating
because it will catch the next author as well: `ISOLATE_CTX` is declared
`thread_local!` while its own doc comment calls it "the per-isolate DB context"
and its type is named `IsolateDbContext`
(`crates/zeroship-plugin-db/src/context.rs:956`; the declaration moved when the
frame-effect work landed, and this citation pointed at unrelated prose until it
was re-derived).
It is per-**thread**. So two isolates on one thread *already* share one context,
and an arm asserting they share one slot passes before the work is done.

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

  Two threads, not two isolates on one thread, and that is the whole point of
  the arm. The contract being tested is that the live cache is *process-wide*;
  a same-thread arm passes identically on a wrong implementation that keeps one
  service and one cache **per thread**, because `ISOLATE_CTX` is thread-local
  (`crates/zeroship-plugin-db/src/context.rs:956`) and both isolates share it
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
  `AppDbBinding` struct) and this contract states two paragraphs above that
  driver resources stay non-`Send` `Rc` deliberately, because they never cross a
  thread. The two pointer checks in this arm are therefore over **different**
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
- Operator deprovisioning performs **no** second URL parse and opens **no**
  second pool.
- **PostgreSQL arm:** a deprovision arriving while an isolate holds a handle does
  not abort an in-flight operation; the next operation for that app fails closed.

  Scoped to PostgreSQL deliberately. SC-2's `DetachApp` **must** abort
  outstanding reservations before restore's file swap (`sc2:115-135`), or a
  connection is left bound to an obsolete inode - so an unscoped no-abort rule
  and SC-2 are jointly unsatisfiable on SQLite. Graceful app deprovision and
  forced file detach are different events and only the first is covered here.
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
- **The env cache is GC'd by app id alone and leaks secrets across
  incarnations.** `envs.retain(|app_id, _| versions.contains_key(app_id))`
  (`crates/zeroship-worker/src/sync.rs:189`) keeps an entry whenever the id is
  still present in the version map, so a same-id recreation **retains the
  previous app's env snapshot and serves it to the new one** until an
  `env_version` bump displaces it. That is a stale-secret leak, not an untidy
  cache, and it sits outside `zeroship-plugin-db` entirely - which is why no
  document in this set named it. The GC identity must include the incarnation.

  Recorded here despite being another subsystem's code, because it is the same
  identity gap and would otherwise be found by whoever first recreates an app id
  in anger. The DB fence closing while the env cache stays open would be a
  fence around the wrong door.
- Two isolates racing their first DB operation on one thread enter one
  initialization singleflight and publish exactly one backend - **asserted with
  a test that fails when the singleflight is removed.** Today's per-thread
  `ISOLATE_CTX` makes the naive form of this arm pass without any singleflight at
  all.
