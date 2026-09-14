# zeroship-workflow-manager

Native platform workflow coordination. This crate owns worker registration,
placement, calendar scheduling, management commands, the durable metadata queue and the deployment
retention ledger through the shared Rust ORM. It has no customer journal,
payload storage or V8 dependency.

`coordinator::Coordinator` uses the queue's bound database and app-scope lock.
Placement changes lock the app scope before the worker row so capacity remains
serialized across apps. Native job operations verify stored assignments and
invoke the host's current enrollment check inside the queue transaction.
Recovery pagination filters existing owners before
ending a result page, so owned scopes cannot hide later recovery work.
Coordinator reads use generated field predicates and native `FromRow` decoding;
the queue's app locks and conditional writes remain the serialization boundary.

`Queue` binds a provisioned platform database. App locks serialize queue changes;
delivery attempts fence retries and stale acknowledgements. Settlement writes
its receipt and successor jobs in the same transaction. Authenticated hosts
revalidate placement through the authorized methods after lock waits and before
commit. Receipt replay requires the original worker's current enrollment.
Timeout during an in-flight commit leaves an uncertain outcome; retries recover
its durable receipt without adding successor jobs again.
Claim and heartbeat return `DeliveryGrant`; converting it to a wire lease after
commit charges elapsed time against the originally observed assignment authority.
Worker publication cannot mint manager-owned activation, cron or management commands.
Job outcomes use closed tagged objects. Management results cannot settle ordinary
jobs, and generic completion cannot stand in for a management lifecycle result.
Control management acceptance persists the raw request, frozen job and run order
in the same transaction. Latest selection observes the ordinary app pointer outside
queue locks, then confirms the selected deployment's exact hold hash. Exact raw
retries resolve from independent request indexes before any source or hold I/O.
Transitions and Started restarts need no deployment selection or queue hold.
Management deliveries follow accepted run revisions. Unsettled pause, cancel and
restart commands suppress Advances for their run before candidate limiting;
resumes and work for other runs continue through ordinary eligibility checks.
Bounded native joins validate pending command, job and order records before claims.
Settlement commits its closed command result and settled revision with the queue
receipt and successors, releasing only that command's provisional barrier. Exact
settled replay validates retained linkage and checks current enrollment after its
metadata reads; it never changes newer barriers.

The app's persistent dispatch cursor assigns tickets to new jobs and successful
claims in their existing transactions. Due and dependency filters run before ticket
ordering, so an expired delivery retries behind work already waiting; new arrivals
cannot continually displace that retry. Due times and job identity remain unchanged.
Exact publication replay, heartbeat and settled acknowledgement replay do not
rotate work. Failed claim transactions roll back both the cursor and job ticket.
Failures before a successful claim still require a host retry or parking policy.

`coordinator::Coordinator::policy_lease` verifies placement and the original
enrolled key before consulting a trusted `policy::PolicySource`. Source I/O runs
outside manager locks. Final admission locks the app before the worker and
rechecks both authorities, preserving the original source and placement
deadlines. The complete raw policy comes from `zeroship_core::workflow_policy`.
Requesting a lease never renews placement or registration. `PolicyGrant` charges
transaction settlement and revalidates the retained source observation when
constructing the response. An unavailable or stale source is an infrastructure
failure; valid disabled policy is returned unchanged.

`policy::control` reads complete app/plan/operator authority from Control's own
schema using native ORM. A durable per-app publication row serializes observers
before their relational input read. Policy and source-validity changes advance its
revision; unchanged input preserves it. The finite observation starts before
database acquisition, and cache hits preserve that original deadline. Failed or
cancelled refreshes cannot restore a retired cache entry. This provides bounded
convergence across manager replicas, without claiming that an operator update
immediately quiesces customer execution. The server uses this provider; ordinary
worker assignment and policy refresh still need production composition.

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
manager. Normal Control deployment publication and ordinary worker composition
still need integration.

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

`driver::Driver` visits independent bounded calendar, reconciliation, collection
and retention pages.
Each lane captures its upper identity and advances past an attempted candidate
before I/O. A shared lane deadline bounds scans and candidate operations; failed
items remain durable and retry after the finite sweep wraps. Cancellation retains
scan progress and rotates the next lane. The host owns cadence and shutdown.
The driver needs no worker registration, placement or creator database. The
retention lane resumes acquiring/releasing intents and releases the queue hold
of a deployment the app's enabled calendar no longer selects once no unsettled
job needs it. A hold becomes eligible only after `Options::hold_grace`, which
`Driver::new` requires to exceed the queue's transaction timeout: an acquirer
confirms its hold outside its transaction, so it commits within that budget
before any pass may release the hold. Each candidate is decided again under the
app lock; a hold still in use stays held for a later pass.

`recovery::Recovery` retains activation provenance in `recovery_scopes` and
independent Reconcile and Collect responsibilities in `recovery_duties`. Trusted
activation establishes both duties atomically. Repeated registration validates the
complete existing pair and preserves each deadline and pending identity; newer
activation changes provenance only. Each publication and its own next deadline
commit together. Waiting accelerates only the matching pending duty; completion
and exact receipt replay preserve periodic responsibility. An unsettled job is reused
across replicas and restarts, so absent workers cannot erase responsibility or
accumulate replacement jobs. Bounded due pages include healthy scopes and must
be swept repeatedly. Host scheduling, capacity provisioning and the ingress
epoch handshake still require integration before enabling customer ingress.

`deployments::DeploymentHolds` belongs in Control or the local platform host.
It records normal app deployments and generation-fenced retention holds. Manager
queue ownership and customer journal ownership require separate host authority;
the journal hold API does not grant manager access.

`local::LocalPlatform` binds the local host's single platform metadata file. Its
explicit SQLite bootstrap installs the deployment catalog and manager schemas
together into an empty file and refuses, without rewriting, any file whose stored
DDL differs from that combined compiler output, including a deployment-only
catalog. The catalog and the queue use separate ORM bindings to the file; the
queue acquires its deployment holds through that catalog.

`deployments::latest::LatestDeploymentSource` observes the ordinary app deployment
pointer through a native ORM join to the same app's catalog row. It selects only
identity, hash and retention state; missing or unavailable targets refuse without
falling back to activation history. The returned value is an observation, with no
admission authority or deployment hold. Ordered management acceptance receives this
source explicitly; platform grants and readiness belong to the host.

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
frontiers of an enabled calendar under the app lock. It validates bounded pages of unsettled job
specifications before ruling out executable dependencies. Recovery provenance
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
Normal Control deployment publication and the production worker consumer still
require cutover.

Run `cargo test -p zeroship-workflow-manager` for PostgreSQL Testcontainers and
SQLite queue, scheduling and retention contracts.
