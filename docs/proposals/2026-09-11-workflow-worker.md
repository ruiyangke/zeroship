# Workflow manager, durable job queue and workers

**Status:** Agreed architecture; implementation requires a scheduling and queue
cutover. This proposal replaces the earlier coordinator-only target in this
file. The server owns scheduling and durable delivery. Workers pull bounded
jobs, execute them against creator storage, commit their outcomes and acknowledge.
The existing branch does not yet implement this complete architecture.

The creator-data boundary remains unchanged: workers access only their authorized
creator databases and object storage. Control and the workflow server access only
the Control database. Production places these processes in separate zones with
private databases. Neither side receives the other side's database credentials.

This proposal supersedes the older
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
The [workflow reference](../reference/workflows.md) describes existing behavior;
this document defines the replacement rather than claiming that it has shipped.

## Architecture

```text
PLATFORM ZONE

  Normal app deploy ---- cron registration -----+
  Explicit start() ----- metadata submission ---+
  Signal / child event - metadata submission ---+
  Management ---------- authorized command -----+
                                                |
                                                v
                              +--------------------------------+
                              | Workflow manager/server        |
                              |                                |
                              | Cron and durable timers        |
                              | Job creation and retry policy  |
                              | Worker assignments and capacity|
                              | Management and delivery state  |
                              |                                |
                              | Durable job queue              |
                              +---------------+----------------+
                                              |
                                              v
                                      Private Control DB
                                      Metadata only

==================== authenticated service API ====================
                  Workers initiate polling and reporting

CREATOR EXECUTION ZONE

                              +--------------------------------+
                              | Worker                         |
                              |                                |
                              | Pull a job                     |
                              | Resolve trusted app context    |
                              | Load journal + pinned app code |
                              | Execute a bounded turn in V8   |
                              | Commit outcome                 |
                              | Report outcome and acknowledge |
                              +----------+----------+----------+
                                         |          |
                                         v          v
                              Private creator DB   Creator storage
                              Inputs, history,     Large payloads
                              checkpoints,
                              execution receipts
```

Manager and server name the same component, hosted by
`zeroship-workflow-server`. The job queue is a responsibility inside that
component, initially persisted through the ORM in the Control database. It does
not require a separate broker or another deployable service. Queue claims and
acknowledgements use the manager API; workers receive no Control DB connection.

A job is a bounded unit of work: accept a scheduled occurrence, advance a workflow
execution, apply a management operation, deliver a customer event, reconcile an
outbox, or collect eligible customer data. A workflow can span many jobs. Waiting
for a deadline or signal releases the execution slot and V8 isolate.

## Role split

| Responsibility | Manager/server and queue | Worker |
| --- | --- | --- |
| Cron | Register schedules, evaluate expressions, persist occurrences and create jobs. | Accept the identified occurrence and execute its app code. |
| Sleep and retry deadlines | Own durable timers and enqueue work when due. | Compute the next execution outcome and persist its checkpoint. |
| Work discovery | Discover runnable queue entries and due platform timers. | Pull an assigned job; do not scan creator databases for due workflows. |
| Placement and capacity | Register workers, assign authorized scopes, retain recovery obligations and request execution capacity. | Renew authority and consume eligible jobs within its capacity. |
| Reliable delivery | Lease jobs, retain pending work, retry delivery and settle acknowledgements. | Fence execution, deduplicate and acknowledge committed outcomes. |
| Workflow semantics | Apply closed scheduling outcomes without reading customer execution data. | Replay code, evaluate business decisions and persist transitions. |
| Management | Accept authorized commands and enqueue them. | Apply lifecycle changes atomically in the creator journal. |
| Customer maintenance | Schedule explicit maintenance and reconciliation jobs. | Execute bounded journal, payload and retention operations for the assigned app. |

The worker has a consumer loop, execution heartbeats and bounded transport retry.
It has no cron evaluator, due-work scanner, app scheduler or independent retention
sweep. A recovery or collection job can inspect its assigned creator journal;
that does not grant the worker responsibility for discovering when to schedule
those jobs. Reconciliation pages yield continuation metadata to the manager.

Control retains creator authorization, deployment publication, runtime policy,
route metadata and deployment retention. Gateway routes requests. Neither runs
workflow scheduling loops or opens creator journals. The manager executes no
creator JavaScript and holds no customer payload credentials.

## Data ownership and contracts

| Storage | Authoritative contents |
| --- | --- |
| Control DB, manager metadata | Worker registrations, assignment revisions, cron definitions, timer deadlines, jobs, delivery attempts, management commands, submission receipts and recovery obligations. |
| Control DB, deployment metadata | Immutable deployment identity, activation state, bundle holds and reclamation state. |
| Creator DB | Workflow inputs, runs, generations, history, checkpoints, waits, signals, execution fencing, committed job outcomes, publication intents and payload references. |
| Creator object storage | Large inputs, signals, results and prepared payload uploads. |
| Normal app bundle store | The existing app manifest, modules, runtime descriptor and content-addressed artifacts. |

Manager messages are closed typed metadata. A job identifies its app, logical
operation, deployment, target run or customer record, expected generation or
revision, availability time and delivery authority. A customer record reference
is an opaque identity, not a database URL, object-store location, signed download
URL or credential. The worker resolves it through its trusted app binding.

Outcomes contain acknowledgement identities and scheduling decisions such as
completed, ready for another turn, waiting until a deadline, waiting for an event,
or a closed rejection category. They contain no arbitrary result or error JSON.
Inputs, history, signal bodies, stack traces and result payloads never enter the
queue, manager logs or manager receipts. Detailed reads terminate at the
customer's authenticated worker endpoint through normal app routing.

Workflow scheduling metadata can include a workflow export identity, cron
expression, timezone and declared execution policy. Registration extracts an
allowlisted descriptor from the normal deployment contract; it does not copy
arbitrary app configuration or embedded inputs into the manager. Business-data
conditions are evaluated by a worker job and produce a typed scheduling outcome.

App identity comes from enrollment, trusted runtime context and authorized
placement, never a body field or creator-supplied `APP_ID`. Service assertions
bind the intended audience and endpoint. The receiver verifies the enrolled
worker, scope, assignment revision and current authority before accepting a
mutation. The queue cannot route an app to an unrelated creator zone.

Native handles also bind the app and resolved database. A caller cannot nominate
another app, physical schema or object namespace. Follow the
[data-system contract](../architecture/data-system.md): schema bindings and database
permissions enforce storage authority. Reserved table names are not a privilege
boundary, and direct ORM access remains governed by the shared ORM contract.

## Durable queue and execution protocol

The queue provides at-least-once delivery with idempotent submission and
acknowledgement. Submission identity is separate from delivery attempt identity.
Repeated submission of the same operation returns its existing receipt;
conflicting content under that identity is refused. A lost delivery response
must not create another logical job.

Manager replicas serialize queue claims and due-schedule mutations through the
Control DB. Advancing a cron cursor and creating its occurrence/job commit
together. Settling a job and creating its follow-up jobs or timers also commit
together. Conditional mutations check affected rows; selecting and later writing
without a concurrency predicate is not a claim protocol.

Successor intents have the same stable identities whether published through a
completion acknowledgement or outbox reconciliation. Manager settlement and
submission deduplicate in the same identity domain and reject conflicting
content. A lost completion reply cannot create duplicate successors through
those publication paths.

```text
Manager/queue                    Worker                    Creator DB
     |                             |                           |
     |<---------- poll ------------|                           |
     |-- job + delivery lease ---->|                           |
     |                             |-- fence / read receipt -->|
     |                             |<-- history or outcome ----|
     |                             |                           |
     |                             | execute pinned app code   |
     |<--------- renew ------------|                           |
     |                             |-- commit checkpoint, ---->|
     |                             |   outcome and intent      |
     |                             |<-- confirmed commit ------|
     |<-- outcome + acknowledge ---|                           |
     |                             |                           |
     | settle job + next schedule  |                           |
     |-- durable receipt --------->|                           |
```

The manager owns the delivery lease. The creator journal owns the fence deciding
which execution may commit customer state. These are different authorities:
reassigning a queue entry alone cannot prevent an old process writing to a
private database. Each attempt validates its authorized app, logical job,
generation, execution revision and bounded deadline in the creator transaction.
A replacement fences the previous execution before publishing new state.

The worker first checks the durable job receipt. If the job already committed,
it reports that outcome without replaying app code. Otherwise it claims the
expected execution frontier, loads the pinned deployment and executes within a
bounded budget. Duplicate jobs targeting the same frontier cannot both advance
it. Receipt retention lasts until an explicit protocol proves redelivery is no
longer possible; ordinary history cleanup cannot erase deduplication authority.

History, run state, confirmed payload references, the job outcome and publication
intents commit together. Customer transaction failures leave the job retryable.
A timeout after an uncertain commit is resolved by reading its receipt. A stale
attempt cannot acknowledge a different attempt or publish unrelated follow-up
work. A current redelivery can settle the same previously committed outcome.

Losing confirmed execution authority stops or quarantines the isolate and joins
native operations before releasing the slot. Deadline checks cover synchronous
JavaScript, storage I/O and transaction settlement. Renewal never revives an
expired execution. This protocol does not promise atomic transactions across the
Control and creator databases or retract an already in-flight database commit.

Delivery retries recover transport and worker failures. Workflow-level retries
follow the declared policy and the worker's committed execution outcome. An
infrastructure failure must not become a catchable business failure that changes
recorded history. The manager cannot infer application retry decisions from a
stack trace or inspect inputs to decide the next action.

## Explicit start and durable submission

`start(input)` uses the app's native workflow handle in its ordinary worker
runtime. Rust platform code running on the creator side can use the same scoped
handle. The input stays in creator storage; the manager creates the queued job
from a metadata submission.

```text
App start(input)
      |
      v
Worker's app-scoped handle
      |
      +--> Creator transaction: run + input reference + submission intent
      |
      +--> Manager: submit stable operation identity + customer record reference
                         |
                         v
                  Queue job + submission receipt
                         |
                         v
                  Worker pulls and executes
```

The customer transaction is durable acceptance. It can return an accepted run
handle after commit; that means accepted work, not completion or immediate queue
visibility. Repeated starts use the same request identity and cannot create a
second run. A full queue or unreachable manager leaves the publication intent
pending instead of undoing an already committed customer acceptance.

A customer outbox alone cannot ensure progress after the last worker disappears.
Before admitting a start or signal, the worker must hold a manager-recorded
scope responsibility that includes recovery of unpublished customer intents.
The manager retains that obligation across registration expiry and reassigns a
bounded reconciliation job when the worker disappears or publication remains
unconfirmed. This is a targeted job for a known app, not a platform journal scan.

Each ingress-enabled scope has a durable manager-owned reconciliation deadline,
even when the manager has no visible pending job. Healthy heartbeats cannot
postpone it indefinitely: the manager cannot observe an unpublished customer
intent. The deadline queues reconciliation after bounded publication retries
fail without a worker crash. Closing ingress and confirming its durable drain
permits retiring that obligation; future ingress must establish it again before
writing customer data. This avoids keeping an idle app worker resident.

A reconciliation job reads that app's pending intents and resubmits them by
stable identity. It acknowledges its cursor only after publication is confirmed;
unfinished pages become continuation jobs. The manager must already own the
recovery obligation before the customer write is allowed. First-time app ingress
therefore establishes that responsibility before accepting data. An unregistered
worker cannot accept work and hope a future outbox scanner finds it.

If no worker is available to receive an input, the platform must arrange
authorized capacity and retry the customer-side request. The metadata-only
manager cannot accept the input durably on the customer's behalf. A request
that never reached creator storage has not been accepted.

Graceful shutdown closes ingress admission, finishes in-flight customer writes
and confirms publication before releasing responsibility. Manager-driven
reconciliation remains necessary for crashes. The manager cannot retire the last
recovery obligation solely because a worker stopped sending heartbeats.

## Cron and deployment activation

The normal deployment process registers cron metadata with the manager. Cron
registration must not depend on an app isolate or an initial HTTP request.
Control sends an authorized deployment revision and its allowlisted schedule
metadata. Publication is durable and idempotent; a lost acknowledgement is retried.

The manager evaluates the cron expression and timezone. Occurrence identity
includes the app, schedule identity, schedule revision and scheduled instant.
Concurrent replicas and recovery from downtime cannot enqueue the same occurrence
as unrelated jobs. Updating the schedule and its next deadline is atomic with
job creation. Misfire and overlap behavior are explicit declared policies;
implicit wall-clock retries must not change their meaning.

Deploy registration prepares schedules for an immutable deployment. An ordered,
idempotent activation protocol selects which revision can produce new occurrences.
Delayed messages from an older deployment cannot reactivate its schedules.
Already queued occurrences retain their selected revision and deployment pin.
Disabling a schedule fences future occurrences and defines cancellation of
unaccepted jobs without silently retargeting accepted runs.

Where execution requires creator-side deployment initialization, the manager
dispatches a bounded activation job before releasing that revision's execution
jobs. The worker verifies the retained artifact, commits its local deployment
state and acknowledges. It does not evaluate cron. Registration can therefore
occur without a resident app worker; due occurrences remain durable while
activation or capacity is pending. Failed activation cannot silently dispatch
against another deployment.

A cron job can create its creator-side run on first execution because it has no
arbitrary input body to transport. Its input is resolved from the pinned app code
or a creator-side reference. The worker records occurrence acceptance and run
creation together; redelivery returns the same run. Overlap decisions requiring
actual customer run state occur in this acceptance transaction and return a
closed outcome to the manager.

Queued occurrences must retain their deployment before any worker accepts them.
Activation and the platform hold ledger protect that dependency; requiring a
customer-journal hold only after delivery would leave a deletion race.

## Sleeps, signals and dependent work

A sleep commits a customer checkpoint and a timer-publication intent. The manager
persists the deadline and creates the next job when due. Timer identity binds its
app, run generation and wait revision. Cancellation, restart or an earlier event
can obsolete the wait; late timer delivery becomes a fenced no-op in the creator
journal. The worker never waits in an isolate for the deadline.

Signals enter through the app's authenticated worker endpoint. It persists the
signal body and a delivery intent in creator storage; the manager receives only
the reference. An event job consumes the signal or advances a waiting workflow
transactionally. Signals arriving before a waiter exists remain durable. Wait
registration checks already stored events, so publication ordering cannot lose
a wake-up. Competing signal and timeout jobs serialize on the same wait revision.

Child completion, topic fanout, continuation and compensation follow the same
pattern: a worker commits the customer transition and scheduling intent, and the
manager creates follow-up jobs. Parent/child references and event cursors stay
app-scoped. Fanout and cleanup operate in bounded pages; the manager queues their
continuations. No worker loop scans for parent waits, due retries or expired
payloads independently of assigned jobs.

## Management and recovery

Control authorizes pause, resume, cancellation and restart. The manager enqueues
a typed operation with a stable request identity and provenance. It does not mark
a customer transition complete merely because it queued the command. A worker
commits the operation and a deduplicated receipt in the creator DB, then reports
a closed result. Missing targets and explicit lifecycle refusals can have durable
receipts; database failures and expired authority remain retryable.

Pause and cancellation also fence new manager deliveries as appropriate, while
already delivered work must observe the customer lifecycle fence. A restart
quiesces the old generation before admitting another. Races with completion,
signals and child jobs resolve in customer transactions; delayed messages cannot
resurrect an obsolete generation or remove an unrelated wait.

Delivery barriers apply to execution jobs, while resume, cancellation and required
reconciliation remain deliverable. A rejected management operation releases its
own provisional barrier. Committed lifecycle revisions bind scheduling changes,
so a delayed acknowledgement cannot clear a newer pause or undo a later command.

When an app has no available worker, due jobs remain durable. The manager must
publish execution demand to deployment infrastructure capable of starting an
authorized worker in the creator's zone. A queue and polling protocol alone do
not implement recovery from zero workers. The capacity adapter and its retry
contract are required production integration, not assumed existing behavior.
No inbound connection to a private creator database or dormant worker is required.

Journal-aware maintenance is also queued work. The manager can schedule a bounded
job for a known app without knowing its payload contents or run history. Worker
crashes retain both queue delivery and app reconciliation responsibility; they
never imply that customer data or deployment references can be discarded.

## Payloads, deployments and upgrades

V8 receives app-scoped primitives and replay data, not task credentials, database
connections or raw storage credentials. Rust owns payload preparation and journal
writes. Large results use creator object storage; prepared uploads keep their
identity across retries. Reference promotion commits with the outcome. Abandoned
uploads remain collectible, and collection serializes with reference promotion.

Replay resolves content through the captured app and generation, verifies it and
enforces resource limits. Missing or corrupt content stops the attempt without
letting app code commit a different history. External effects can repeat when a
remote effect succeeds before its checkpoint commits. Stable effect identities
support destination idempotency; queue deduplication is not exactly-once execution
of arbitrary external effects.

Workflow classes use the normal app `.zship` artifact. A run generation pins an
immutable deployment ID and the normal manifest's content hash. The shared bundle
loader verifies the modules and runtime descriptor. Missing code parks the job;
it never falls back to the latest deployment. There is no workflow-only archive,
executable snapshot store, upload flow or `--workflow-bundle` option.

Deployment retention has platform queue consumers and creator journal consumers.
The manager retains deployments referenced by pending jobs and schedules. Workers
report journal dependencies through durable app-scoped holds, including retained
history and restart generations. Delivery acknowledgement alone cannot release a
hold needed for replay. Platform collection consults its own ledger, active
routing and other deployment consumers, never creator SQL.

Hold acquisition and reclamation serialize on the deployment record. Admission
requires confirmed acquisition. Before releasing a journal hold, an assigned job
closes admission for that deployment and verifies no remaining customer references
under the app lock, then records release intent. Hold generations fence stale
retries and reacquisition. Holds survive placement expiry, crashes and manager
restart. A lost release acknowledgement retains the artifact until reconciled.

New runs use the active deployment; existing generations keep their pins. Full
restart can explicitly select another deployment and re-execute the original
input. Partial restart retains the deployment that produced its preserved history.
Mid-run upgrade remains a proposed explicit checkpoint handoff, not a shipped API
or automatic consequence of redeploying. It requires quiescence, a bounded
business-state transformation, target retention and a customer transaction that
fences the source generation. Signal, child and compensation carry-over semantics
must be defined before enabling it. No live-tenant compatibility layer is needed.

## Rust composition and ORM

`zeroship-workflow` contains reusable native contracts, customer journal operations,
replay types and bounded job execution. `zeroship-workflow-v8` is the thin binding
and V8 executor. `zeroship-worker` composes trusted app context with the job
consumer and execution runtime. `zeroship-workflow-server` hosts the manager and
metadata queue. Manager scheduling belongs to reusable native code so local
development can embed it without a daemon or a second scheduler implementation.
No new crate is required merely to name each responsibility.

The existing `WorkflowService` is a Rust library object. Its current scheduling
methods do not justify scheduling in the worker: split manager scheduling from
customer transition operations and remove the old worker maintenance loop.
Runtime-loader interfaces should reflect actual I/O; use a synchronous interface
when construction only assembles an already loaded app. Neither `async-trait`
nor a separate runtime is an architectural requirement. Shipped I/O stays on compio.

Both sides consume `zeroship-data-orm` with their own host-bound connections.
Worker journal operations use the normal creator database and object store.
The ORM handles PostgreSQL and SQLite selection, connection ownership, query
compilation and transaction settlement. Prefer model and collection operations;
keep raw SQL only for concrete public-API gaps such as database clock access.
Do not restore separate workflow PostgreSQL and SQLite journal backends.

The canonical migration DSL generates schemas and ORM descriptors. Rust model
mapping uses the shared `schema!`, `FromRow`, `Insertable` and `Changeset`
contracts. Every table has `id` as its sole primary key. Composite domain
identities use unique indexes and complete predicates, not composite primary
keys. Preserve conditional updates, app and generation scope, rollback on
cancellation and confirmed commit semantics.

Provisioning applies creator migrations on the creator side and platform
migrations on the platform side. Claiming a job creates no schemas, roles or
tables. Worker-owned writes use ordinary DML, with no privileged wrappers or
Control DB grants. Shared ORM changes stay with the ORM owner; integrate main's
supported API rather than introducing workflow-only access exceptions. Existing
migration TS definitions can change directly before launch.

## Local development

The CLI composes the same manager/queue and worker libraries in process. The
worker still consumes jobs through the shared contract, and the manager owns cron
and timers. Local calls can avoid HTTP and enrollment ceremony without bypassing
queue receipts, execution fencing or durable publication. No workflow daemon or
external broker is needed for ordinary local development.

Customer history remains in the app's normal database; payloads use normal app
storage. The embedded manager uses the normal local platform metadata/catalog
store alongside deployment identities and holds, with separate bindings. It does
not introduce a workflow-specific SQLite file, database environment variable or
customer-DB connection in the manager. Local co-location does not change the
production privilege split.

`zeroship serve` and Vite use the normal app bundle and resolved configuration.
The CLI is a thin single-app composition root: setup, startup and shutdown. It
contains no bespoke cron logic, scheduler, workflow bundle handling or deployment
watcher. Programmatic Rust configuration and TOML resolve the same host settings.
The host supplies trusted app identity; creators need not provide `APP_ID`.

Restart preserves queue metadata, journal state, publication intents and retained
bundles. Hot reload activates a new immutable deployment while previous runs keep
their code. Shutdown drains or abandons leased jobs through the shared protocol
and preserves recovery responsibility. Supporting multi-app CLI deployments is a
separate question.

## Implementation and verification

### Existing work and required changes

The branch already has a customer-bound ORM journal, replay and lifecycle
operations, payload handling, a bounded runner, a V8 adapter, normal bundle
loading, deployment holds and a registry/placement/management server. The CLI
composes the journal and runner. These are reusable foundations, not evidence
that the manager-owned job architecture is complete.

The existing replacement worker discovers due work and runs maintenance itself.
The coordinator stores wake hints rather than owning durable job delivery. Those
responsibilities must change. Existing production paths also still contain
Control journal access, worker platform-table joins, incompatible grants and
Control/Gateway workflow-advance dispatch. Their removal is required to establish
the zone boundary. Current source behavior remains documented as current until
the corresponding producer/consumer cutover is complete.

Keep customer transaction protections, scoped native handles, prepared payloads,
replay, deployment pins and the V8 shutdown barrier. Replace the worker's scheduler
and local task discovery with queue delivery and job-driven operations. Coordinator
wire types, migrations, fixtures and callers change together; obsolete APIs are
removed, without aliases or a parallel legacy mode.

### Work sequence

- Define closed submission, job, delivery, outcome and acknowledgement contracts,
  including logical identities, app scope, revisions and retention of receipts.
- Implement manager queue persistence, replica-safe claims, timers, cron and
  atomic settlement of follow-up work through native ORM operations.
- Connect normal deployment publication to schedule registration, activation and
  queue-owned deployment retention. Define misfire and overlap behavior explicitly.
- Adapt customer journal acceptance, fences and receipts to delivered jobs. Add
  durable submission and outcome intents with manager-owned scope recovery before
  allowing customer ingress; prove lost-publication recovery without a resident
  app worker.
- Compose a bounded worker consumer with the trusted app binding, pinned bundle
  loader and V8 executor. Replace worker due-work and maintenance loops with jobs.
- Route signals, child events, management and maintenance through durable intents
  and manager delivery. Keep business payloads and detailed reads customer-side.
- Integrate assignment recovery and the execution-capacity adapter. Demonstrate
  pending jobs can obtain an authorized worker after capacity reaches zero.
- Embed the shared manager and consumer in the existing local host, keeping CLI
  setup thin and storage configuration ordinary.
- Remove the old Control/Gateway advancement APIs, creator-journal access from
  platform services, platform read grants from workers and obsolete configuration.
  Replace journal-reading bundle retention with the hold ledger.
- Reconcile with main's ORM changes and complete native integration and example
  coverage before reporting production cutover complete.

### Required evidence

| Contract | Required evidence |
| --- | --- |
| Database zones | Run services with private, disjoint database access. Workers cannot query Control tables; platform services cannot connect to creator storage. |
| Simple consumer | With worker scheduling disabled, manager-generated cron, timer, retry and maintenance jobs execute through the consumer. |
| Queue durability | Concurrent managers, cancellation, restarts and lost replies preserve logical jobs and allow redelivery. |
| Submission recovery | Lose publication after customer commit while heartbeats continue, and crash before enqueue with loss of the last worker; manager-issued recovery jobs publish the accepted intents. |
| Completion recovery | Crash after customer commit and before acknowledgement; redelivery returns the receipt without rerunning the committed turn. Race acknowledgement with outbox publication without duplicating successors. |
| Cron activation | Deploy without a resident app worker, reorder registration messages, update timezone/policy and restart the manager without duplicate or retargeted occurrences. |
| No active capacity | A due job remains durable and requests authorized worker capacity; no poll from a nonexistent process is assumed. |
| App isolation | Reject foreign app selectors, references, deployment IDs, generations and forged runtime context through authenticated APIs and native handles. |
| Execution fencing | Expired delivery, reassignment, synchronous app code and delayed writes cannot publish a stale frontier or settle another attempt. |
| Events and lifecycle | Signal-before-wait, timeout races, parent completion, cancellation and restart preserve durable events and reject stale generations. Pause still permits management; rejected or delayed commands cannot strand delivery barriers. |
| Payload integrity | Failed uploads, uncertain commits, corrupt reads and concurrent collection cannot admit invalid references or delete retained content. |
| Bundle retention | Pending cron jobs and creator history both retain code. Acquisition, deletion, delayed release and placement loss cannot reclaim a live dependency. |
| Local parity | The embedded manager and worker use the same queue, scheduling, receipt, payload and replay contracts as production. |

Native Rust tests own engine, manager and worker verification through the workflow
xtask. Required external environments belong to Testcontainers with major image
tags; unavailable databases fail required tests rather than skip them. Customer
journal contracts run on PostgreSQL and SQLite. Each example owns its Vitest and
Playwright tests and fixtures. No bash workflow test orchestration is introduced.
