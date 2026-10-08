# zeroship-workflow-manager

Native platform workflow coordination. This crate owns the durable metadata
queue and the zone claim workers pull from, calendar scheduling, management
commands, recovery responsibility, each execution zone's capacity target and the
deployment retention ledger, through the shared Rust ORM. It has no workflow
journal, payload storage or V8 dependency.

`Queue` binds a provisioned platform database. Each app has one `queue_scopes`
row, which carries the app's frozen execution zone, its persistent dispatch
cursor and the app lock every queue change serializes on. `register_scope_in`
records the zone the first lifecycle message names and refuses a later one naming
another zone as a conflict; concurrent first registrations of one app converge on
one row, and registering a scope also ensures its zone's capacity target row.
Delivery attempts fence retries and stale acknowledgements: heartbeat, settle and
give-back compare the job, worker and attempt the latest claim wrote. Authorized
hosts revalidate enrollment through the authorized methods after lock waits and
before commit. Settlement persists the outcome the journal decided and publishes
no successor; a creator journal's committed intents reach the queue through
`Queue::submit`, which refuses `Management` and `Close` operations because only
the manager's own operations mint those. Receipt replay requires the original
worker's current enrollment. Timeout during an in-flight commit leaves an
uncertain outcome; retries recover its durable receipt.

`coordinator::Coordinator::claim_in_zone` is a worker's claim. The host passes
the zone frozen on the authenticated instance row, and the claim pages the apps
of that zone that hold a claimable row (`scheduling::claimable_apps_in_zone`),
distinct apps in app id order after the request's cursor and without the apps the
request excludes. A request without a cursor starts at a worker-stable rotation of
the first page, so workers do not all start at the same app. Each lap takes at
most one job per app, and laps continue while the previous one delivered
something, until the request's slots are filled or its attempt deadline passes:
`coordinator::Options::claim_budget`, or the wait the request states less the
share kept back for the reply (`REPLY_RESERVE_DIVISOR`), whichever ends first
(`Coordinator::claim_deadline`). Every give-back the claim makes ends by half
that reserve later and never waits for an app's lock, so the reply leaves inside
the wait. A grant whose lease its host's admission spent is not given back,
whatever the admission answered: it is reported lapsing, and its row lapses and
is redelivered once the stored deadline passes, counting no attempt and no
back-off. A request above `ClaimJobs::MAX_DELIVERIES` deliveries or
`ClaimJobs::MAX_EXCLUDE` exclusions is refused before any work, and a
transient failure to check the caller's enrollment is that app's skip, not the
request's failure.
An app is passed, and named in the `ClaimReport`, when its policy observation is
unavailable, when `PolicyObservation::admits_zone` refuses it (deleted, or of
another zone), when its policy disables admission or dispatch or sets
`max_running` to zero, when its live `advance` leases are at `max_running`, when
another session holds its scope locked, or when nothing in it is claimable after
barriers and the delivery ceiling. The concurrency cap is counted before the lock
and again under it, so two workers cannot both lease an app's last slot. The
reply carries the grants, the cursor the
next claim continues from and whether this claim reached the end of the zone.
`Claimant::Worker` admits only `advance` and `Claimant::Maintenance` every other
kind (`Claimant::admits` in `src/models.rs`), so a worker never takes a sweep. On
PostgreSQL the zone claim locks the app's scope with `for_update_nowait`, which
the manager reports as `Error::Contended` and the zone claim skips; SQLite, whose
write lock is database-wide, keeps the update-as-lock `lock_scope`. Every other
claim - `Queue::claim_authorized`, which the maintenance lane, management and
closing reach through `MaintenanceAuthority` - waits for the lock.

Claim and heartbeat return `DeliveryGrant`; converting it to a wire lease after
commit charges elapsed time against both the lease and the attempt. A heartbeat
never extends a lease past `leased_at` plus `Options::max_attempt`, which
`Options::validate` requires to be at least the lease. Redelivery is bounded by
the app policy's `max_delivery_attempts`, which the caller supplies from its own
policy authority: a job whose counted executions reached it is not a
candidate. An attempt is counted once, on the first evidence that it began: its
first renewal, or its holder's report that it was interrupted
(`GiveBack::Interrupted`). Counting it resets the job's consecutive back-offs.
Nothing settles an exhausted job; no executor produced an outcome for it, and the
row keeps its attempt history and so keeps its deployment. What keeps a hung run
from arriving there is the creator engine, which counts the dispatches of one
frontier that reported nothing and settles the run `stalled`; a terminal run then
settles its delivery job through the ordinary path. That verdict is only
reachable while this ceiling still admits deliveries, which is why `AppPolicy`
refuses a strike limit at or past it.

`Queue::give_back` returns a live delivery the holder cannot use to `ready` in
the same transaction rather than leaving it leased until the lease lapses. It
clears the holder, sets `jobs.deferred_until` from the `GiveBack` it is given -
an exact instant, a fixed pause, or a back-off that doubles with each consecutive
back-off up to `Options::defer_backoff_max`. Only a back-off counts itself in
`jobs.deferrals`, the count the next back-off doubles by, and a deferral whose
instant has already passed leaves the row claimable at once. An interrupted
attempt is claimable at once and counted; an unsent delivery is claimable at
once and counts nothing. Claimable rows, the claim page and the capacity census
all
filter on `deferred_until`; `available_at` stays immutable eligibility metadata.
A give-back never counts toward `max_delivery_attempts`.

Job outcomes use closed tagged objects. Management results cannot settle ordinary
jobs, and generic completion cannot stand in for a management lifecycle result.
Control management acceptance persists the raw request, frozen job and run order
in the same transaction. A Latest restart names its deployment in the request,
and acceptance confirms that deployment's exact hold hash. Exact raw retries
resolve from independent request indexes before any hold I/O.
Transitions and Started restarts carry no deployment and need no queue hold.
Management deliveries follow accepted run revisions. Unsettled pause, cancel and
restart commands suppress Advances for their run before candidate limiting;
resumes and work for other runs continue through ordinary candidate checks.
Bounded native joins validate pending command, job and order records before claims.
Settlement commits its closed command result and settled revision with the queue
receipt, releasing only that command's provisional barrier. Exact settled replay
validates retained linkage and checks current enrollment after its metadata
reads; it never changes newer barriers.

The app's persistent dispatch cursor assigns tickets to new jobs and successful
claims in their existing transactions. Due and dependency filters run before ticket
ordering, so an expired delivery retries behind work already waiting; new arrivals
cannot continually displace that retry. Due times and job identity remain unchanged.
Exact publication replay, heartbeat and settled acknowledgement replay do not
rotate work. Failed claim transactions roll back both the cursor and job ticket.

`policy::PolicySource` is the trusted policy authority every claim, capacity
visit and run call reads through. A `PolicyObservation` carries the app's
revisioned `AppPolicy`, its frozen execution zone and its deletion marker, all
from one source read, and a finite validity that cached reads preserve.
`PolicyObservation::admits_zone` is the one rule the run path and the claim path
share: a deleted app is refused to every zone and a live app is served only by
its own zone. An unavailable or stale source is an infrastructure failure; valid
disabled policy is returned unchanged.

`policy::control` reads complete app, plan and operator authority over Control's
app-facts capability and the service's own publication ledger. A durable per-app
publication row serializes observers before their input read. Policy and
source-validity changes advance its revision; unchanged input preserves it. The
finite observation starts before database acquisition, and cache hits preserve
that original deadline. Failed or cancelled refreshes cannot restore a retired
cache entry. This provides bounded convergence across manager replicas, without
claiming that an operator update immediately quiesces customer execution. The
server composes this provider for its HTTP threads and, separately, for its
driver. `local::ConfiguredPolicies` is the local host's authority: its one
configured app and policy, in the default zone, never deleted.

`scheduling::Scheduler` prepares immutable schedule metadata from normal app
deployments. Activation selects future scheduling and commits its job and recovery
responsibility together. Due dispatch persists occurrence identities, queue jobs
and the catch-up cursor in one transaction. Page limits preserve the original
catch-up boundary and remaining allowance. Occurrences wait for their own
activation job to complete; replacement stops future generation without changing
already queued jobs. Claims and receipt replay verify stored job linkage, and
frontier extension checks the immutable descriptor and calendar interpretation.
Creator activation and cron acceptance are handled by the customer engine.
`Scheduler::selection` reads the app's current revision, enabled state and
selected activation, including whether that activation job settled as completed.
The server and the local CLI host drive due scheduling through the native
manager, and Control's lifecycle publisher delivers registration, activation and
disable, each naming the app's zone.

Calendar disable shares the platform's activation revision sequence. Its durable
receipt remains replayable after restore, and disabling before the first
activation fences delayed older requests. Disable retains accepted jobs, calendar
progress and recovery responsibility. Frozen frontiers retain no code, so the
driver releases the disabled deployment's queue hold once its jobs settle.
Restoring the same deployment acquires the hold again and preserves its interval
anchor and catch-up progress while creating new activation readiness. Due scans
exclude disabled scopes before applying their page limits.
This gate controls calendar publication; creator admission and executor shutdown
have separate policy authority.

`driver::Driver` visits independent bounded scheduling, reconciliation,
collection, retention, closing and capacity pages (`TickReport::lanes`).
Each lane captures its upper identity and advances past an attempted candidate
before I/O. A shared lane deadline bounds scans and candidate operations; failed
items remain durable and retry after the finite sweep wraps. Cancellation retains
scan progress and rotates the next lane. The host owns cadence and shutdown.
The driver needs no worker and no creator database. The
retention lane resumes acquiring/releasing intents and releases the queue hold
of a deployment the app's enabled calendar no longer selects once no unsettled
job needs it. A hold becomes a release candidate only after `Options::hold_grace`, which
`Driver::new` requires to exceed the queue's transaction timeout: an acquirer
confirms its hold outside its transaction, so it commits within that budget
before any pass may release the hold. Each candidate is decided again under the
app lock; a hold still in use stays held for a later pass. The same lane carries
that deployment's journal release duty once its queue hold is released: on the
same terms it publishes one `ReleaseHold` job asking the creator engine to check
its own journal and give the deployment back, and applies that job's settled
reply. A refusal returns the duty to pending until another grace has passed, and
reacquiring the queue hold clears a duty already in flight. The closing lane
abandons an app Control deleted (`Recovery::abandon`): its duties go, and its
unsettled creator jobs are settled `Rejected` in the same transaction, because no
worker may run a deleted app.

`capacity::Capacity` keeps one declarative target per execution zone, in
execution slots. Each visit reads the zone's apps that hold an unsettled creator
row through the driver's policy source, outside any transaction lock, so an idle
scope costs nothing: an admitted app's demand is its live
leases plus as many claimable rows as its `max_running` leaves room for, counted
with the same predicate the claim uses. Exhausted rows, rows inside a give-back
back-off and every row of an app whose policy withholds dispatch or which Control
deleted never count toward demand; a complete visit records them on the target
row beside the backlog depth and the oldest claimable row's availability. A
visit cut off by its deadline leaves the zone's cycle where it stopped and the
next visit continues it, so a zone too large for one visit is measured over
several; a cut visit may raise the target and never lowers it, and the visit
that reaches the end completes the cycle, whose census has measured every app
once and may lower it too. An unavailable observation freezes the target. The
target's revision advances only when the
desired slots change; a fall applies only after the hold-down. The provider
receives `CapacityRequest { zone, revision, desired_slots }` and answers
`Accepted` or a closed `Refused` reason. `LocalCapacity` accepts every target and
`StaticPool` refuses exactly a target above its `pool_slots`. A request is sent
when the target changes, while it is requesting, and again once its
`retry_interval` has passed, so a steady target resynchronizes.

`recovery::Recovery` retains activation provenance in `recovery_scopes` and
independent Reconcile and Collect responsibilities in `recovery_duties`. Trusted
activation establishes both duties atomically. Repeated registration validates the
complete existing pair and preserves each deadline and pending identity; newer
activation changes provenance only. Each publication and its own next deadline
commit together. Waiting accelerates only the matching pending duty; completion
and exact receipt replay preserve periodic responsibility. An unsettled job is reused
across replicas and restarts, so absent workers cannot erase responsibility or
accumulate replacement jobs. Bounded due pages include healthy scopes and must
be swept repeatedly. The workflow service establishes ingress epochs through
`Recovery::establish` before accepting creator ingress and reports accepted
ingress through `Recovery::note_ingress`.

`deployments::DeploymentHolds` belongs in Control or the local platform host.
It records normal app deployments and generation-fenced retention holds. Manager
queue ownership and journal ownership require separate host authority;
the journal hold API does not grant manager access.

`local::LocalPlatform` binds the local host's single platform metadata file. Its
explicit SQLite bootstrap installs the deployment catalog and manager schemas
together into an empty file and refuses, without rewriting, any file whose stored
DDL differs from that combined compiler output, including a deployment-only
catalog. The catalog and the queue use separate ORM bindings to the file; the
queue acquires its deployment holds through that catalog.

A Latest restart carries the deployment it replays against, as
`ManagementOperation::Restart { deployment }`. The manager reads no app pointer
of its own: the only caller of ordered management acceptance is the authority
for that pointer. What acceptance does enforce is the hold. An unheld deployment
takes the acquisition path, whose row lock on Control's catalog denies another
app's deployment and refuses one that has left `available`; a held one must
carry the hash Control minted with the hold, and a request naming a different
hash is a conflict. Acceptance does not check that the named deployment is the
app's current one, because the caller decided that.

`retention::HoldClient` supplies the queue's Control capability. Activation,
execution, cron and resolved Latest restart jobs require a confirmed deployment hold.
Reconciliation, collection, lifecycle transitions and Started restart jobs do not
depend on a queue deployment. Started restart prerequisites belong to creator
storage; creator delivery and collection handlers have their own implementation
boundaries. The operation carries executable identity; decoding stored jobs verifies
the kind, run, request and nullable deployment projections against its immutable
specification digest. The queue persists
acquire/release intents and generations; network requests run outside its database
transactions. Release closes publication and checks pending jobs and the
frontiers of an enabled calendar under the app lock. It validates bounded pages
of unsettled job specifications before ruling out executable dependencies. Recovery provenance
does not retain code. Completed receipts may
replay after code reclamation. A host must reconcile unfinished intents, and
Control reclamation must consult the shared ledger before deleting manifests.

`schema/` and `schema/deployments/` record definitions through the migration DSL
and generate migration metadata and dialect DDL. Rust ORM models use native
`schema!` declarations, checked against that metadata in tests. Runtime queue and
retention operations use collections and models. Database clock queries and local platform
bootstrap remain explicit host operations.

The native library is composed by the workflow server and by `zeroship serve`
under the [workflow proposal](../../docs/proposals/2026-09-11-workflow-worker.md).
Workers reach it only through the server's authenticated routes.

Run `cargo test -p zeroship-workflow-manager` for PostgreSQL Testcontainers and
SQLite queue, scheduling and retention contracts.
