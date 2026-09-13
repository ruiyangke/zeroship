# Workflow manager, durable job queue and workers

**Status:** Agreed architecture with protocol decisions still identified below.
Implementation is in progress. Native coordinator and queue operations share ORM
transactions. Regression tests have exposed a concurrent registration conflict;
the schema correction and server integration are being verified.
Manager scheduling, creator outbox publication and the simple worker consumer
have not completed their production cutover.

The manager owns when work becomes runnable and how it is delivered. An ordinary
app worker pulls an authorized job, executes a bounded operation against creator
storage, commits the result and acknowledges closed metadata. A workflow can
outlive the worker, isolate and delivery attempt that advanced it.

The principal rule is process ownership: **workers access only authorized creator
databases; Control and the workflow manager access only the Control database.**
Production places these processes in separate zones with private databases.
Neither side receives the other side's database credentials. Customer inputs,
history and results stay in the creator zone throughout the protocol.

This proposal supersedes the earlier
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
The [workflow reference](../reference/workflows.md) describes existing APIs;
statements marked as target or required work here are not claims of shipment.

Read by concern:

- [Components and private zones](#components-and-private-zones),
  [worker identity and placement](#worker-identity-registration-and-placement).
- [Storage and transactions](#storage-inventory-and-schema-ownership),
  [delivery protocol](#durable-job-protocol), [lifecycle state](#lifecycle-state-and-authority).
- [Trigger sequences](#trigger-lifecycles),
  [recovery and capacity](#recovery-responsibility-and-execution-capacity).
- [Deployments and retention](#deployment-pins-upgrades-and-retention),
  [payloads and effects](#payloads-effects-and-collection).
- [Crates and APIs](#rust-apis-and-dependency-direction),
  [configuration and local development](#configuration-and-local-development).
- [Operations](#operations-security-and-backpressure),
  [failure and verification](#failure-behavior-and-verification),
  [remaining work and decisions](#implementation-progress-and-remaining-decisions).

Terms used throughout:

| Term | Meaning |
| --- | --- |
| Scope | The app whose jobs and creator binding an operation may use. |
| Run | A durable invocation of a workflow, independent of the process executing it. |
| Generation | A run's execution incarnation, with its own pinned code and replay history. Restart creates another generation. |
| Frontier | The current committed point from which execution may advance; its revision changes when the journal advances. |
| Job | A durable instruction to perform a bounded operation. A run can require successive jobs. |
| Attempt | A particular delivery of a job to a worker. Redelivery changes the attempt, not the job's meaning. |
| Fence | An identity or revision checked by a write so an obsolete execution cannot modify current state. |
| Receipt | A durable record of an accepted operation or committed outcome, used to answer retries consistently. |
| Publication intent | A creator transaction's durable instruction to publish metadata after commit; stored in an outbox. |
| Recovery responsibility | The manager's obligation to revisit an app even when it cannot see unpublished creator work. |
| Deployment hold | A durable reference preventing normal bundle reclamation while queued work or creator history still needs its code. |

## Components and private zones

```text
PLATFORM ZONE

  Creator management / normal deployment
                  |
                  v
            +-----------+        metadata         +---------------------+
            | Control   |------------------------>| Workflow manager    |
            |           |<--- scoped hold API ----|                     |
            | Authz     |                         | Cron and timers     |
            | Deploys   |                         | Placement/recovery  |
            | Retention |                         | Management commands |
            +-----+-----+                         | Durable job queue   |
                  |                               +----------+----------+
                  |                                          |
                  +---------------> Private Control DB <-----+
                                       metadata only

=================== authenticated service API ========================
                Workers initiate polling and reporting

CREATOR EXECUTION ZONE

  Normal app request                    Outbound poll / submit / ACK
          |                                          |
          v                                          v
  +------------------------------------------------------------------+
  | Ordinary zeroship-worker                                         |
  |                                                                  |
  | App-scoped start / signal / reads     Bounded job consumer         |
  |                                        |                         |
  | Trusted app context -> customer engine -> pinned app code in V8    |
  +-----------------------+-------------------------+------------------+
                          |                         |
                          v                         v
                 Private creator DB          Creator object storage
                 Runs, history, waits,       Inputs, outputs, signals,
                 receipts and outbox         prepared payload objects
```

`zeroship-workflow-server` is the deployable manager host. Its queue is persisted
in the Control database through the ORM. It introduces no broker service or
separate workflow-worker executable. Deployment infrastructure starts ordinary
`zeroship-worker` processes; the manager requests capacity through an injected
host adapter.

The worker retains a consumer loop, execution heartbeats and bounded transport
retry. It has no cron evaluator, due-work scanner or independent maintenance
scheduler. Reconciliation and collection can read the assigned creator journal,
but they run because the manager delivered a bounded job. Waiting for a signal,
child or deadline releases the execution slot and isolate.

| Component | Owns | Excludes |
| --- | --- | --- |
| Control | Creator authorization, enrollment, normal deployment publication, policy, routing metadata and deployment retention APIs. | Creator journal access and workflow advancement loops. |
| Manager | Calendar evaluation, durable jobs and deadlines, placement, delivery attempts, command delivery, recovery responsibility and capacity demand. | Customer code, payload credentials and creator DB connections. |
| Worker customer engine | App-scoped acceptance, replay, lifecycle decisions, journal fences, inputs, history, results and durable publication intents. | Platform tables and scheduling discovery. |
| V8 adapter | Creator-facing handles and execution of the selected app deployment under resource limits. | Queue credentials or direct manager persistence. |
| Gateway | Normal routing and request authentication. | A workflow scheduler, journal reader or private workflow-advance control channel. |
| Deployment host | Starting eligible workers and supplying trusted app context in the proper execution zone. | Deciding customer workflow transitions. |

The workflow subsystem checks app authority. Existing creator and app-request
authorization still applies at their entrypoints; the queue adds no end-user
permission model. An app-scoped native handle cannot select another app's journal,
schema or object namespace. Follow the
[data-system contract](../architecture/data-system.md). Table naming and schema
visibility are not platform authorization: creator-owned rows cannot grant
placement, turn admission on or manufacture a platform deployment hold.

## Worker identity, registration and placement

Registration records a running process; it does not start a process. Enrollment,
liveness, app placement and execution fencing answer different questions.

```text
Deployment host          Worker              Control                 Manager
      |                     |                    |                       |
      |--- start ---------->|                    |                       |
      |                     |-- enroll key ----->|                       |
      |                     |<-- instance ID ----|                       |
      |                     |                    |                       |
      |                     |----- signed registration ---------------->|
      |                     |                    |       verify enrolled |
      |                     |                    |       instance/key;   |
      |                     |                    |       record capacity |
      |                     |<---- registration acknowledgement --------|
      |                     |                    |                       |
      |                     |                    |-- authorized scope -->|
      |                     |                    |       select eligible |
      |                     |                    |       worker placement|
      |                     |----- poll assigned work ----------------->|
      |                     |<---- job + delivery authority ------------|
      |                     |                    |                       |
      |                     | execute against creator DB/storage        |
      |                     |----- committed outcome + ACK ------------>|
```

| Authority | Record or capability | Meaning |
| --- | --- | --- |
| Enrollment | `zeroship.worker_instances` and the instance signing key. | Control authorized this instance identity; the presented key must match an active enrollment. |
| Workflow registration | `workflow_manager.workers`. | The instance reports capacity, ready/draining state and liveness. |
| Scoped assignment | `workflow_manager.assignments`. | A particular worker may consume this app's work under a revision and expiry. |
| Delivery | A leased manager job and its attempt. | This instance may process this particular job under the current placement. |
| Creator execution fence | Customer journal claim, generation and frontier revision. | This execution may publish the next customer transition. |

A worker uses its enrolled instance key for service assertions. The receiver
verifies issuer, audience, permitted endpoint, assertion lifetime and replay
protection before reading the buffered request. A worker ID in JSON is only a
selector. A generic worker-role signer cannot substitute for an enrolled
instance key. Every transport retry uses a fresh assertion and the same durable
operation identity.

The manager admits placement only within platform-authorized app and execution
zone eligibility. Spare capacity is not authority to serve any app. Eligibility
must come from trusted deployment configuration or Control metadata; workers
cannot nominate database locations or broaden eligibility by registration.
The worker also verifies that its locally resolved creator binding matches the
assigned app. Routing and database credentials cannot be supplied by the job.

Registration renewal does not revive an expired assignment. Assignment revisions
and released rows are retained so delayed renewals cannot recreate old authority.
Capacity admission is serialized across apps assigned to the same worker.
Ready workers may receive new placements; draining workers may finish authorized
work and reconcile committed outcomes without being selected for new placement.

Freshness checks belong around the work they authorize. Authenticate at ingress,
then recheck enrollment and stored placement after waiting for locks and before
admitting the mutation to commit. Reuse the verified request context for these
checks rather than consuming its assertion replay identity again. An unavailable
enrollment source is retryable infrastructure failure, not a durable customer
rejection. Existing placement TTL checks do not replace enrollment revocation.

On graceful shutdown the worker reports draining, closes local admission and
finishes or abandons bounded execution through the protocol. A crash expires
liveness and delivery authority while durable work and scope responsibility
remain. Healthy registration does not discharge an obligation to recover an
unpublished creator intent.

**Implementation boundary:** Control enrollment already runs at worker startup.
The workflow registration endpoint and authenticated client exist. Production
startup registration, consumer wiring, zone eligibility and capacity activation
remain cutover work. Bootstrap credentials must not allow a revoked deployment
to restore authority by enrolling a fresh identity. The approved bootstrap trust
source, replacement authorization and revocation freshness contract must be
finalized with the authentication owner; registration alone does not solve them.

## Storage inventory and schema ownership

### Platform metadata

The native manager schema is generated by
[`workflow-manager/schema/schema.ts`](../../crates/zeroship-workflow-manager/schema/schema.ts).
The production namespace is `workflow_manager` in the private Control database.
The server migration consolidates the former `workflow_coordination` namespace
with this queue namespace; it is not a second authoritative placement store.

| Table | Stored authority and purpose |
| --- | --- |
| `workflow_manager.schema_version` | Generated schema fingerprint. Runtime roles read it; provisioning owns changes. |
| `workflow_manager.queue_scopes` | Registered apps and the shared app lock for queue and coordinator operations. |
| `workflow_manager.workers` | Instance liveness, capacity, ready/draining state and serialization of worker-wide admission. |
| `workflow_manager.assignments` | App/worker placement revision, expiry and release tombstone. Existing wake-hint fields belong to the coordinator protocol being replaced. |
| `workflow_manager.placement_receipts` | Immutable assignment/release request identity and recorded result. |
| `workflow_manager.management` | Authorized command metadata, request provenance and reported closed outcome. |
| `workflow_manager.jobs` | Immutable job specification, availability, current attempt, delivery fence and settlement digest/outcome. It currently also supplies submission and settlement deduplication. |
| `zeroship.worker_instances` | Control-owned enrollment, public key and revocation state; distinct from workflow registration. |
| `zeroship.app_deploys` | Control-owned immutable deployment metadata and reclamation state. |
| `zeroship.app_deploy_holds` | Control-owned app/deployment/holder generation and retention state. |

The scheduling target also requires durable schedule revisions and cursors,
occurrence identities, deployment activation, scope recovery epochs/deadlines,
capacity demand and management delivery barriers. Their physical model is not
implemented by the current table list. Prefer extending the owning manager
models over adding another store. Final table names and the shared protocol
fields must be selected together; an in-memory map is not a substitute.

A delayed job's `available_at` is sufficient for a simple timer. Separate timer
rows are warranted only where calendar cursors, cancellation or coalescing need
additional durable state. Do not create a scheduler database alongside the queue.

### Creator journal

The journal is generated by
[`workflow/schema/schema.ts`](../../crates/zeroship-workflow/schema/schema.ts).
The following names are in the app's resolved creator schema. Every name in this
table has the `__zeroship_workflow_` prefix; none belongs in Control's schema.

| Table suffix | Customer-owned content and target treatment |
| --- | --- |
| `schema_version` | Creator journal schema fingerprint, installed by creator-side provisioning. |
| `app_state` | App serialization and journal counters. Trusted admission policy remains outside customer SQL. |
| `deploys` | Locally accepted immutable app deployment and availability state. |
| `deployment_holds` | Customer dependency intent and observed hold generation; not the platform hold ledger. |
| `runs` | Run identity, lifecycle state, current generation, relationships and frontier metadata. |
| `generations` | Pinned deployment, input, output/error references and generation lifecycle. |
| `steps` | Replay history, checkpoints and compensation state. |
| `tasks` | Existing customer execution claims, fences and completion receipts. Adapt to delivered jobs rather than use it as a second scheduler. |
| `waits` | Recorded sleep, signal and child waits and their execution scope. |
| `topics`, `broadcasts`, `signals`, `subscriptions` | Customer event bodies, ordering, targets, subscription state and fanout cursors. |
| `requests` | App-operation request deduplication and its declared retention policy. |
| `management_receipts` | Durable creator outcomes keyed by management request, separate from expiring app requests. |
| `schedules`, `occurrences` | Existing customer schedule definitions and accepted occurrences. Calendar discovery moves to the manager; customer acceptance, overlap state and input references remain customer-side. |
| `payloads`, `payload_refs` | Prepared upload metadata, ownership, integrity and committed references. |
| `outbox` | Existing customer event outbox. Extend or replace its contracts for durable queue publication; its present existence does not mean that cutover is complete. |

Delivered-job receipts, publication intents and execution fences need stable
job/frontier identities independent of transport attempts. They may use adapted
journal tables or dedicated receipt models. Their exact schema is pending; the
transaction and retention rules below are required regardless of placement.
There is no manager reader of these tables. A reconciliation job reads them
through an app-bound worker, never through a platform connection.

Creator object storage holds large inputs, signals, results and prepared uploads.
The normal bundle store holds `.zship` manifests and modules. Neither is a place
to copy customer data for scheduling. An opaque payload ID in a job is not a
signed download URL, database URL, object key or credential.

### Models, migrations and transaction domains

Every table has `id` as its sole primary key. Composite domain identities use
unique indexes: for example app/worker, app/request and app/run/generation.
Scoped foreign keys retain the app identity. IDs and cursors preserve bytewise
ordering. New rows receive typed storage IDs; upserts preserve existing storage
IDs, revision tombstones and immutable receipts.

An app scope uses its `AppId` directly as `queue_scopes.id`; a worker registration
uses its `WorkerId` as `workers.id`. Dependent `app_id` and `worker_id` foreign
keys reference those primary keys. Do not duplicate the same identity in another
uniquely indexed parent column: competing first-registration upserts must share
the same conflict arbiter. Records with a composite domain identity, such as an
assignment, retain their own storage ID and composite unique index.

The canonical migration DSL generates SQL and ORM descriptors. Both platform
and creator persistence use native `zeroship-data-orm` models and collection
operations, including `schema!`, `FromRow`, `Insertable` and `Changeset` where
appropriate. PostgreSQL/SQLite selection belongs to the ORM. Do not restore
separate workflow backends or add workflow-specific ORM access exceptions.

Provisioning applies each schema in its own authorized zone before execution.
Claiming a job performs ordinary DML and creates no schemas, roles or tables.
Platform startup still verifies restricted role membership, required grants,
read-only fingerprints and lack of customer authority. Connection authority is
a binding mode, not proof that the supplied role is appropriately restricted.

The manager's coordinator and queue use the same physical namespace and callback
`Database` transaction. Internal placement checks receive that existing handle.
Opening another coordinator transaction from a queue callback could wait on its
own app lock and would separate authorization from mutation. Preserve
app-lock-before-worker-lock ordering, and validate affected rows on fenced writes.
Capacity counts cover the full matching set. Recovery pagination filters owners
before ending a result page, so an owned page cannot conceal later missing owners.

| Transaction | Operations that commit together |
| --- | --- |
| Manager placement | Scope registration, capacity admission, assignment revision and placement receipt. |
| Manager submission | Stable job specification, submission deduplication and owning scheduling/recovery metadata. |
| Manager calendar turn | Selected occurrences/jobs and the schedule cursor/revision that produced them. |
| Manager settlement | Exact delivery fence, immutable outcome, successor jobs and associated scheduling/barrier changes. |
| Creator acceptance | Input/event reference, run or signal acceptance, request result and publication intent. |
| Creator execution | Frontier transition, history, waits/children, promoted payload references, committed job outcome and successor intents. |
| Creator management | Lifecycle transition or explicit refusal, request digest, durable outcome and resulting publication intents. |
| Control retention | Deployment reclamation fence and hold mutation under the same deployment lock. |

There is no distributed transaction across these owners. Durable intents,
idempotency and reconciliation connect their commits. Object uploads likewise
cannot join a database transaction; reference promotion supplies that boundary.

Database clocks decide local expiry and due work. The manager uses its own
independent clock connection to the same Control database, sampled after lock
waits; it does not query through the transaction's held pool connection. Creator
fences use the creator database and trusted host policy. Raw clock reads and
host privilege inspection are narrow public-API gaps, not alternate SQL backends.

A monotonic caller budget includes lock acquisition, authority checks and waiting
for commit. Clock conversion includes elapsed clock-query and transport time;
later checks can shorten an attempt's budget but cannot restore elapsed time.
The deadline expires execution and new mutation admission. Cancellation before
terminal dispatch must roll back. A timeout after COMMIT dispatch is ambiguous:
settlement may finish, and the caller must read its durable receipt. It is never
proof that the transaction rolled back. Cross-zone clock skew and transfer of
remaining lease authority are a protocol decision still called out below.

## Durable job protocol

### Identities and allowed metadata

The current closed contract lives in
[`workflow_jobs.rs`](../../crates/zeroship-core/src/workflow_jobs.rs):
`JobSpec`, `JobOperation`, `Delivery`, `Settlement` and `SettlementReceipt`.
It defines advance, cron, management, reconciliation and collection operations.
`JobOutcome` contains `Completed`, `Waiting` and `Rejected`; successor jobs carry
further availability. These types are foundations, not a complete activation,
event-delivery or paginated-maintenance protocol.

Keep the following identities distinct:

| Identity | Retry rule |
| --- | --- |
| App request | Reuse for retried start/signal/management acceptance; changed body conflicts. |
| Logical job | Reuse across publication retries, successor submission and delivery attempts. |
| Assignment revision | Changes when placement changes; registration renewal cannot restore an old revision. |
| Delivery attempt | Changes on redelivery; a stale attempt cannot settle the new attempt. |
| Run generation/frontier | Fences customer history and identifies the next admissible transition. |
| Wait/event/occurrence | Identifies the semantic trigger even if several delivery attempts observe it. |
| Hold generation | Fences retention acquisition/release independently of task and assignment leases. |

Bodies and nested variants are closed and bounded. The manager receives only
allowlisted scheduling metadata: app, job, deployment, run/reference identity,
revision, operation, deadline and closed outcome. Workflow export identities,
cron expressions, timezone and declared scheduling policy can be allowlisted
deployment metadata. Arbitrary inputs, result JSON, signal bodies, stack traces,
customer connection details and free-form error messages never cross this API.
Detailed run reads terminate at an authorized worker through normal app routing.

An identifier parsed from a body grants no authority. Host authentication selects
the worker and allowable app; all referenced jobs, deployments and successors
must agree with that scope. Unknown or cross-app references must not become a
probe into another tenant's state through differing payloads or diagnostics.

### Delivery, execution and settlement

```text
Manager job:

  durable ready -- available_at reached + authorized claim --> leased
       ^                                                    /      |
       |                         expiry / recoverable loss /       |
       +-------------------------------------------------+        |
                                                        exact ACK |
                                                                  v
                                                               settled
                                                                  |
                                           immutable replay <-----+

Creator transition:

  delivered job -> existing committed receipt? -> report same outcome
                        |
                        no
                        v
              claim expected frontier
                        |
              execute bounded operation
                        |
              commit state + receipt + intents
                        |
              report closed outcome / publish intents
```

```text
Manager/queue                  Worker                       Creator DB
      |                           |                              |
      |<---- authenticated poll --|                              |
      | lock app; verify current placement/enrollment             |
      | persist delivery attempt |                              |
      |---- job + authority ---->|                              |
      |                           |-- receipt/fence transaction ->|
      |                           |<-- prior outcome or claim ----|
      |                           |                              |
      |                           | load pin; execute bounded turn|
      |<---- heartbeat ----------|                              |
      |---- bounded authority -->|                              |
      |                           |-- checkpoint + receipt ------>|
      |                           |   + payload refs + intents    |
      |                           |<-- confirmed commit ----------|
      |<---- outcome + ACK -------|                              |
      | lock app; verify fence; settle + insert successors        |
      |---- settlement receipt -->|                              |
      |                           |-- mark publication confirmed ->|
```

The queue provides at-least-once delivery. It deduplicates immutable submission
and settlement content, including the successor set. A heartbeat can update the
stored deadline; the mutable echoed deadline is not part of immutable delivery
identity. Retrying after a lost heartbeat reply must remain possible.

Renewal cannot revive expired execution. A retry can observe a still-live stored
manager lease, but the worker rejects renewal after its local guard has expired
or been cancelled. A later lease never extends the original execution deadline.
Redelivery requires a fresh attempt and admission, including a check for a
previously committed customer outcome.

The creator journal checks a committed receipt before running app code. A job
that already committed returns that result. Otherwise the worker claims the
expected app/run/generation/frontier, captures trusted policy and pins execution
authority. Competing jobs for the same frontier cannot both advance it. A new
attempt must not reuse another attempt's write authority.

The manager's lease alone cannot stop an old process writing a private creator
DB. Creator transactions check the execution fence and current frontier, and a
replacement serializes with any earlier transaction on that frontier. If an
older COMMIT was already dispatched, recovery reads its result after settlement;
it cannot assume the older execution did nothing merely because delivery expired.
Synchronous JavaScript is interrupted through the execution budget. Quarantine
prevents isolate reuse and initiates cancellation; it does not prove that native
work has stopped. The executor must join native operations through runtime
shutdown before releasing the slot or publishing completion. Rejection of a
JavaScript promise alone is insufficient. Already dispatched COMMIT remains
supervised until its outcome is settled or explicitly uncertain.

A fresh delivery may find a customer outcome committed by an older attempt. It
settles that semantic outcome using its own current delivery fence. The stored
creator outcome must therefore be independent of the old delivery envelope.
Conversely, exact replay of an already settled manager attempt returns its
immutable receipt after placement expiry, but still requires current enrollment
of the original worker. Changed content, another worker or a superseded unsettled
attempt cannot use receipt replay to admit new writes.

### Publication and receipt retention

Successor identities and content are generated once in the creator transaction.
ACK publication and outbox reconciliation use the same IDs and immutable
specifications. They may race; manager submission and settlement share the same
deduplication domain. Neither path substitutes a fresh ID after a lost response.

Creator state commits before its ACK. If the manager is unavailable or full,
intents remain pending. Marking publication confirmed happens only after a
manager receipt is validated against app, job and content. A lost confirmation
write merely causes another idempotent publication attempt.

App request receipts and delivery receipts have different lifetimes. Existing
`requests` retention does not authorize deleting durable management outcomes,
job receipts or unpublished intents. Until an explicit retirement protocol proves
that submissions and redeliveries are no longer admissible, retain deduplication
records. Expiry of a response cache must not admit a still-retryable accepted
start as a different run. History collection cannot erase the only proof that a
job already ran.
Receipt retirement/watermarks are unresolved; arbitrary TTL-based deletion is not
the default design.

### Lifecycle state and authority

These states describe different objects. A settled queue job can leave its run
waiting for a later job; a live worker does not imply a running workflow. Manager
observations of customer lifecycle are reported metadata, never permission to
reconstruct or overwrite creator history.

| Object and owner | State and transition responsibility |
| --- | --- |
| Worker registration, manager | Ready admits placement; draining stops new placement. Expiry removes live eligibility without deleting pending work or recovery responsibility. |
| Assignment, manager | An active revision can renew until expiry or explicit release. Reassignment advances the revision; retained tombstones fence delayed messages. |
| Job, manager | Pending work becomes deliverable at its deadline, receives a leased attempt, and settles against that attempt. Expiry permits redelivery of the same job. |
| Run generation, creator | The journal owns `queued`, `running`, `sleeping`, `waiting`, `paused`, `stalled`, `compensating`, `completed`, `failed` and `cancelled`. The existing lifecycle rules determine legal transitions. |
| Publication intent, creator | A committed pending intent remains recoverable until a matching manager receipt confirms publication. An unknown remote result remains pending. |
| Management request, both owners | Manager acceptance creates delivery responsibility. Creator application/refusal produces the durable outcome; manager confirmation reports that outcome and settles the matching barrier. |
| Deployment, both owners | Platform activation selects code for new work; creator activation confirms local prerequisites. A run's existing pin changes only through an explicit lifecycle operation. |

The run-state vocabulary lives in
[`workflow_coordination/lifecycle.rs`](../../crates/zeroship-core/src/workflow_coordination/lifecycle.rs).
The customer lifecycle engine owns its transition matrix. Moving scheduling to
the manager does not give queue handlers a parallel run-state machine. In
particular, job `Completed` means the bounded operation committed; it does not
necessarily mean the workflow returned a terminal result.

Terminal completion and collection are separate. A terminal generation can still
retain history, restartable checkpoints, dependent child results, payloads and
deduplication receipts. Management acceptance likewise does not promise immediate
quiescence: its creator transition and fence determine when cancellation or pause
has actually taken effect.

## Trigger lifecycles

Each trigger follows the commit and delivery rules above. The origin determines
which owner first has durable work and which database can contain its data.

| Trigger | First durable write | Manager receives | Worker commits before ACK |
| --- | --- | --- | --- |
| Explicit start | Creator run, input reference, request receipt and intent. | Stable advance job and run identity. | Next frontier, receipt and successor intents. |
| Cron/interval | Manager occurrence and job under activated schedule revision. | Normal deployment schedule metadata. | Occurrence acceptance and run creation, or a durable overlap refusal. |
| Sleep/retry | Creator checkpoint, wait/retry state and timed successor intent. | Delayed job with expected generation/frontier. | Resume transition or stale-wait no-op receipt. |
| Signal/topic event | Creator event body, target/cursor and intent. | Opaque event or fanout reference. | Consumption/fanout page, affected frontiers and intents. |
| Child/continuation | Creator relationship, child/continuation acceptance and intent. | Stable run/event references. | Child progress or parent notification consumption. |
| Management | Manager authorized command and delivery barrier/job. | Closed command with stable request and provenance. | Lifecycle outcome, durable receipt and resulting intents. |
| Reconciliation/collection | Manager scope obligation or maintenance deadline. | Known app and bounded operation/cursor. | Publication/collection progress and continuation intent. |

### Explicit start and input acceptance

`start(input)` runs through the app-scoped native handle in its ordinary runtime.
Rust code in the creator zone can call that same handle. Platform services do
not import it to write customer data.

```text
App code              Worker / creator DB                 Manager
   |                           |                             |
   |-- start(input, request) ->|                             |
   |                           |-- establish scope duty ---->|
   |                           |<-- durable responsibility ---|
   |                           |                             |
   |                           | prepare input; transaction: |
   |                           | run + request result + intent|
   |                           | COMMIT                      |
   |<-- accepted run handle ---|                             |
   |                           |-- submit same job ID ------->|
   |                           |<-- submission receipt -------|
   |                           | mark intent confirmed        |
   |                           |                             |
   |                           |-- poll -------------------->|
   |                           |<-- advance job --------------|
   |                           | commit bounded turn          |
   |                           |-- outcome + ACK ------------>|
```

Creator commit is durable acceptance. The returned handle means accepted work,
not completion or immediate queue visibility. Request retries return the same
accepted run; a changed input under the same identity conflicts. If input upload
or the creator transaction fails, there is no accepted run. If the response is
lost after commit, the request receipt resolves the uncertainty.

Before accepting a start or signal, the worker must hold a manager-recorded scope
responsibility covering unpublished intents. A scope-registration record without
a recovery deadline and drain protocol is insufficient. This obligation is
established before customer acceptance, so failure between databases can leave
extra reconciliation responsibility but cannot leave accepted work undiscoverable.

If no worker can receive the request, the platform requests authorized capacity
and retries the customer-side ingress through normal routing. The metadata-only
manager cannot durably accept the input on behalf of the creator. A request that
never reached creator storage is not accepted.

### Deployment schedules, activation and cron

Normal deployment publication registers an allowlisted schedule descriptor with
the manager without requiring an app isolate, HTTP request or resident worker.
It contains immutable deployment identity, workflow export, schedule identity and
revision, timezone, timing and declared policy. Static business input remains in
the pinned bundle or creator storage; do not send the existing arbitrary
`ScheduleRegistration.input` field as manager metadata.

```text
Control                  Manager / Control DB              Worker / Creator DB
   |                               |                                |
   |-- register deployment ------->| persist prepared revision        |
   |-- activate revision --------->| fence old scheduling revision    |
   |                               | retain deployment               |
   |                               | queue activation if required    |
   |                               |<------------ poll --------------|
   |                               |---------- activation ---------->|
   |                               |                     verify bundle;
   |                               |                     commit deploy state
   |                               |<--------- activation ACK -------|
   |                               | mark dispatch prerequisite ready|
   |                               |                                |
   |                       calendar selects scheduled instant        |
   |                       transaction: occurrence + job + cursor    |
   |                               |<------------ poll --------------|
   |                               |------------ cron job ---------->|
   |                               |                     accept occurrence;
   |                               |                     resolve local input;
   |                               |                     commit run + intent
   |                               |<--------- outcome + ACK --------|
```

Preparation, activation and dispatch readiness are separate states. Activation
selects the revision allowed to produce new occurrences. Delayed deployment
messages cannot reactivate an older revision. Creator-side activation verifies
the retained ordinary bundle and initializes required local deployment state;
it is a bounded job, not a worker calendar loop. Until required activation is
confirmed, due occurrences can be durable but are ineligible for execution.
Failed activation never silently selects another deployment.

Occurrence identity binds `(app, schedule, schedule revision, scheduled instant)`.
The manager persists any generated job/run IDs with that identity before delivery.
Replicas advancing a schedule lock its state and commit the occurrence/job with
the cursor. They cannot mint unrelated jobs for a retry. Already created jobs
keep their selected revision and deployment even when a newer schedule activates.

Keep the existing declared policy vocabulary while moving its evaluation:

| Policy | Required meaning |
| --- | --- |
| `allow` | Occurrences may create overlapping runs, subject to ordinary app admission limits. |
| `skipIfRunning` | Customer acceptance checks the logical schedule's nonterminal runs under the app lock and records a skipped occurrence if it overlaps. Redelivery cannot retry that skip as a new run. |
| `skip` catch-up | Preserve the existing current-due-occurrence behavior, then advance beyond the captured evaluation boundary rather than replaying the intervening backlog. |
| `backfill { max }` | Consider overdue instants from the stored cursor in order, bounded by the declared policy and host ceiling; after exhausting that catch-up allowance, advance beyond the captured evaluation boundary. A processing-page limit must not masquerade as a semantic skip. |
| Interval anchor | Retain the declared epoch or deployment anchor; restarting a process does not reset the schedule. |

The existing calendar resolves an ambiguous local time to its earlier instant
and a nonexistent local time to the next valid local time. Preserve this explicit
behavior during extraction unless the schedule contract is deliberately changed.
Use IANA timezones and stored UTC instants. Nominal times that resolve to the same
instant cannot create duplicate occurrence identities. A timezone or calendar
interpretation change requires an explicit revision policy; persisted occurrences
are never recomputed into different jobs. The timezone-data upgrade policy remains
to be finalized before manager scheduling ships.

The manager does not infer overlap from stale completion metadata. Customer
acceptance serializes occurrence deduplication, overlap/admission, run creation
and input references. Capacity or infrastructure failure remains retryable; it
must not be persisted as an overlap skip. No arbitrary cron input crosses zones.

Disabling a schedule fences future generation. Whether it also withdraws already
queued, unaccepted occurrences is an explicit API choice still unresolved;
accepted runs retain their normal lifecycle. Queue-owned deployment retention
starts before dispatch, not after the worker first opens its journal.

### Sleeps, workflow retries and bounded turns

```text
Worker / Creator DB                          Manager / Control DB
        |                                             |
        | commit checkpoint + wait/retry + intent      |
        |-- ACK or publish delayed successor -------->|
        |<-- durable receipt --------------------------|
        | release execution slot                       |
        |                                             |
        |                               available_at becomes due
        |-- poll ------------------------------------>|
        |<-- job for expected generation/wait ---------|
        | validate wait; commit next turn or stale no-op|
        |-- outcome + ACK ---------------------------->|
```

A bounded turn ends at a replay frontier, durable wait, terminal result or host
budget boundary. The journal records which business retry policy applies and
computes the resulting delay. The manager stores the typed deadline and delivers
the next job; it does not inspect an exception body to choose business behavior.
Retry identity includes the expected generation and frontier so redelivery cannot
increment a workflow retry counter again.

Budget exhaustion cancels the attempt; only explicitly committed checkpoints
survive. It cannot manufacture a workflow failure or a continuation checkpoint
when no customer transition committed.

Transport/worker failure causes delivery retry of the same logical job. An
application failure that the engine intentionally records can produce a new
workflow retry intent. An infrastructure error, expired authority or missing
artifact must not become a catchable business failure that changes history.
The host bounds retries and uses backoff; accepted work remains durable even if
no immediate execution is possible.

A timer binds the app, generation and wait revision. Signal, cancellation,
restart or child completion may obsolete it. A late timer is checked against
customer state and yields an idempotent stale result; it cannot reopen an old
wait. There is no resident isolate sleeping until the deadline.

### Signals and topic fanout

```text
App ingress          Worker / Creator DB                    Manager
     |                         |                               |
     |-- signal(body, ID) ---->| durable scope duty already held|
     |                         | commit signal + intent        |
     |<-- accepted ------------|                               |
     |                         |-- event reference ----------->|
     |                         |<-- submission receipt --------|
     |                         |-- poll ---------------------->|
     |                         |<-- event/fanout job ------------|
     |                         | consume or persist fanout page|
     |                         | + frontiers + continuation    |
     |                         |-- outcome + ACK -------------->|
```

Signal bodies and topic subscription details remain customer data. Direct event
jobs reference a stored signal; fanout jobs reference a stored broadcast and a
bounded cursor. A worker cannot use those references outside the assigned app.
Target generation and wait revision are verified before consumption.

Signal-before-wait is safe because events are durable and wait registration checks
already stored events in the creator transaction. Event and timeout delivery
serialize on the same wait; the winner records the transition and the other sees
a satisfied or obsolete wait. Event delivery order cannot be implemented solely
by arrival order at the manager.

Topic fanout captures its subscription cutoff and commits progress with created
deliveries and publication intents. A retry resumes that cursor without duplicate
semantic delivery. New subscriptions cannot retroactively join an already
captured broadcast. Continuation jobs keep processing bounded; the queue needs
closed event/cursor fields before this flow can replace existing local fanout.

### Child workflows, continuation and compensation

```text
Parent worker / Creator DB                   Manager                 Child job
          |                                    |                        |
          | commit child run + parent wait     |                        |
          | + child scheduling intent          |                        |
          |-- publish/ACK successor ---------->|                        |
          |                                    |<--------- poll --------|
          |                                    |------ child job ------>|
          |                                    |        commit child outcome
          |                                    |        + parent-event intent
          |                                    |<----- outcome + ACK ---|
          |<------- parent notification job ---|                        |
          | commit parent wait consumption     |                        |
          |-- outcome + ACK ------------------>|                        |
```

The parent/child relationship, input and result are creator-side records. Creating
the child and parent wait is atomic with its scheduling intent. Child completion
records a parent notification intent in the same transaction as its result.
The parent job reads that result locally and verifies the parent generation and
wait, so a delayed child cannot advance a restarted parent.

Continuation and compensation use the same pattern: a committed semantic
transition produces stable successors. Partial progress and retry state live in
the journal. Cascading cancellation and large dependency sets yield bounded
continuation jobs rather than an unbounded worker loop. All relationships remain
within the assigned app; cross-app workflow calls are ordinary authenticated app
integration, not a bypass around these journal boundaries.

### Management and delivery barriers

```text
Creator -> Control              Manager                       Worker / Creator DB
          |                        |                                  |
          |-- authorized command ->| commit command + job + barrier    |
          |<-- accepted receipt ---|                                  |
          |                        |<---------------- poll ------------|
          |                        |--------------- command ---------->|
          |                        |                     check receipt/policy;
          |                        |                     quiesce/fence as needed;
          |                        |                     commit transition + outcome
          |                        |<--------- outcome + ACK ----------|
          |                        | settle job + command outcome      |
          |                        | + revision-scoped barrier changes |
          |-- read status -------->|                                  |
          |<-- closed result ------|                                  |
```

Control authorizes pause, resume, cancellation and restart. The manager records
typed commands with stable request identity and provenance. An accepted management
receipt means durable delivery responsibility; only the creator receipt proves
that the lifecycle operation was applied. Detailed customer state remains a
worker read.

The creator engine first matches the complete request digest. Matching requests
return the recorded closed outcome; changed bodies conflict without altering the
receipt. Explicit lifecycle refusals may be durable `NotFound`, `Conflict` or
`Denied`. Database errors, capacity exhaustion and expired/missing host authority
remain retryable and create no permanent refusal. In particular, expiry of an
admission-policy lease is not equivalent to valid policy with admission disabled.

Pause/cancel/restart can provisionally block conflicting execution jobs. Management
and required reconciliation remain deliverable so the barrier cannot prevent
its own resolution. Barrier ownership includes command identity and lifecycle
revision. A rejected command releases only its own provisional barrier; a delayed
ACK cannot clear a newer pause or undo a later command. Creator fences remain
authoritative for work already delivered when the barrier was installed.

Resume requires current authorized admission. Restart quiesces the source
generation before creating another; generation changes fence timers, children
and stale completions. Atomic queue/command/barrier settlement and the required
lifecycle revision fields are cutover work; the existing separate command ACK
surface does not yet encode this complete protocol.

`ManageRun` currently contains no deployment while `JobSpec` requires one.
Management dispatch must resolve the deployment prerequisite from trusted platform
metadata or use a deliberately defined operation-specific requirement. The manager
must not query the customer journal to fill that gap. Exact activation and
management envelope changes remain an explicit protocol decision.

## Recovery responsibility and execution capacity

A creator outbox cannot recover itself when its last worker disappears. The manager
therefore owns a durable obligation for every ingress-enabled app, established
before accepting customer data. It survives registration expiry, deployment
replacement and manager restart.

```text
Creator commit exists; publication was lost
                    |
                    v
Manager scope obligation + reconciliation deadline
                    |
          +---------+--------------------+
          |                              |
   eligible worker exists        no eligible worker exists
          |                              |
          |                     persist capacity demand
          |                              |
          |                     host starts authorized worker
          |                              |
          +<--------- enrollment / registration / placement
          |
          v
Deliver bounded reconciliation job
          |
Worker reads its app's pending intents -> submit stable identities
          |
Commit confirmed progress -> ACK + continuation or next recovery deadline
```

Each obligation has a manager-owned deadline even when no job is visibly pending.
Healthy heartbeats cannot postpone it indefinitely because the manager cannot
observe an unpublished customer commit. Bounded immediate publication retry is
an optimization; periodic manager-issued reconciliation supplies correctness.
The current `queue_scopes` row and recovery scan of missing owners do not yet
implement this durable deadline/epoch protocol.

A reconciliation job processes an app-scoped page. It confirms manager receipts
before advancing publication state and yields continuation metadata when needed.
A scan of newly inserted intents must not permanently skip work behind its cursor;
use stable ordering and a captured boundary, then schedule another pass. Manager
recovery pagination also must reach missing owners beyond fully owned pages.

Retiring a responsibility requires closing ingress for its scope epoch, fencing
new acceptance and proving the creator drain/publication state. Failed closure
keeps the obligation. A last worker's release, an empty local queue or a successful
heartbeat is not that proof. Future ingress must establish responsibility again
before writing customer data. The drain handshake and its closed evidence fields
remain a required protocol definition.

The capacity adapter takes trusted app/zone/deployment demand and returns durable
provisioning progress or a retryable refusal. Requests are idempotent across
manager replicas; provider failure keeps jobs and demand pending. The adapter
starts the ordinary worker and supplies authorized creator connectivity through
the normal deployment host, not through a queue message. The provider integration
and zone-eligibility source are unresolved; no new workflow infrastructure service
is assumed.

Manager maintenance schedules journal reconciliation, payload collection and
customer retention checks as explicit jobs. Such a job examines customer records
only inside the worker. Its failure retains responsibility and references; it
does not permit platform SQL to inspect the journal or declare the app drained.

## Deployment pins, upgrades and retention

Every run generation pins an immutable deployment ID and the normal manifest
content hash. The worker loads the ordinary `.zship`, verifies app/deployment
identity, module graph and runtime descriptor, and uses trusted policy and storage
bindings. Missing or corrupt code leaves durable work unavailable; it never falls
back to the latest deployment. There is no workflow-only archive, executable
snapshot store, upload flow or `--workflow-bundle` option.

A normal redeploy changes the deployment for new runs and activated schedules.
Existing generations replay against their pins. Full restart can deliberately
select another deployment and re-execute the original input. Partial restart
retains the deployment that produced its preserved history. Retained step effects
and payload references copy with their complete app/generation scope.

Mid-run upgrade is a separate proposed checkpoint handoff, not an automatic
consequence of redeploy and not a shipped API. It requires quiescence, retained
target code, bounded business-state transformation and a creator transaction
fencing the source generation. Signal, child, wait and compensation carry-over
must be defined before enabling it. It is not required for the queue cutover.

### Distinct queue and journal dependencies

```text
Manager schedules / pending jobs -- queue holder -----+
                                                      |
                                                      v
                                             Control hold ledger
                                                      |
Creator history / live generations -- journal holder -+
                                                      |
                                                      v
                                             Normal bundle retention
```

The manager acquires queue retention before making a schedule or job executable.
The creator engine acquires journal retention before accepting a dependency needed
for execution or replay. A delivery ACK can discharge queued work while customer
history still needs the deployment. These holders use distinct authenticated
classes and generation sequences.

Control derives holder identity from the authenticated service role and scoped
app. A body cannot choose another holder. Replacement workers preserve the stable
journal holder; manager replicas share their stable logical queue holder. Releasing
a queue dependency cannot release a journal dependency, or vice versa. The current
worker hold API and `HoldScope::for_app` are not yet authorization for queue-owned
manager holds; extend that contract explicitly.

Control remains the production owner of hold acquisition and reclamation APIs.
Its native ledger lives in `zeroship-workflow-manager::deployments` for reuse by
Control and the local host. Constructing the manager queue does not open deployment
catalog tables; the server injects the scoped remote Control hold capability.
The customer engine has only its intent state and metadata client capability,
with no dependency on the platform ledger implementation.

Acquisition and reclamation serialize on the deployment record. Confirm retention
before admitting a new dependency. Before releasing a journal hold, a bounded
worker job closes admission for that deployment, checks all customer dependencies
under its app lock, and commits release intent. Generation tombstones reject stale
release/reacquire messages. Queue release similarly accounts for every referencing
schedule/job under manager serialization.

A lost acquire reply causes an idempotent retry before admission. A lost release
reply retains intent until reconciliation; it must not turn into a new release
generation. Reclamation never reads customer journals, and placement expiry or
worker death never proves that code is unreferenced.

## Payloads, effects and collection

V8 receives app-scoped primitives and replay data. Rust owns task credentials,
trusted app binding, payload preparation and journal writes. The manager never
receives inline input/output bodies, storage URLs or credentials.

```text
Worker                            Creator storage            Creator DB
   |                                     |                       |
   |-- reserve stable preparation / cleanup identity ----------->|
   |<-- confirmed reservation -----------------------------------|
   |-- upload prepared object ---------->|                       |
   |<-- verified identity ---------------|                       |
   |-- confirm prepared object --------------------------------->|
   |                                                             |
   |-- commit outcome + promote payload references ------------->|
   |<-- confirmed commit ----------------------------------------|
   |                                                             |
   |-- later collection job: fence eligibility ------------------>|
   |-- delete collectible object ------->|                       |
   |-- confirm collection progress ----------------------------->|
```

Preparation preserves a stable upload identity across retries and records enough
integrity metadata to verify reads. A durable reservation before upload lets
recovery find abandoned preparations. Upload failure cannot produce a committed
reference. Outcome, history and reference promotion commit together; an uncertain
commit is resolved before treating a prepared object as abandoned. Collectors
serialize eligibility with promotion so they cannot delete newly retained content.
A crash between object deletion and confirmation is handled by idempotent deletion
and retry of the collection job.

Input acceptance may prepare storage before its final transaction. It must still
obey app ownership, upload limits and durable cleanup responsibility for abandoned
preparations. Payload collection is a manager-issued bounded operation, and cursor
progress commits with its continuation intent. Terminal history, child results,
restart prefixes and pending receipts retain their referenced payloads until their
own retention rules permit release.

Replay verifies app, generation, object identity, content integrity and resource
limits. Missing or corrupt content stops the attempt without allowing different
history to be committed. External effects can repeat if a remote operation
succeeds before its checkpoint commits. Stable effect identities support
idempotency at the destination; queue deduplication does not provide exactly-once
arbitrary external effects.

The replay contract stays in the customer engine and SDK. A durable step reuses
its committed result on replay; an uncommitted step can execute again. Workflow
code must preserve the recorded ordering and identity of durable operations for
its pinned generation. A mismatch stops progression instead of silently accepting
a different history. Wall-clock values, random values and external responses
needed for replay must enter through recorded operations. The manager transports
no replay history and does not interpret application exceptions or compensation
bodies.

## Rust APIs and dependency direction

The native manager is reusable without an HTTP listener. The customer engine is
reusable without the manager implementation or V8. The service client carries
metadata and authentication, not either side's persistence.

| Crate | Responsibility and native surface |
| --- | --- |
| `zeroship-core` | Closed workflow metadata and transport-independent capability contracts; canonical entity identities are supplied by `zeroship-id`. No ORM, HTTP implementation or customer replay envelopes. |
| `zeroship-workflow` | `WorkflowService`, bound `AppWorkflows`, creator journal transitions, replay, payloads, bounded job acceptance/execution and publication intents. `apply_management` is a customer operation. |
| `zeroship-workflow-manager` | `Queue`, native `coordinator::Coordinator`, scheduling/recovery modules and platform `deployments` ledger. No creator engine, V8 or listener. |
| `zeroship-workflow-client` | `WorkerCoordinator`, `ControlCoordinator` and bounded authenticated transport, extended with the job/hold protocol as callers cut over. No ORM or scheduler. |
| `zeroship-workflow-v8` | `WorkflowBinding`, V8 argument conversion, trusted app binding and executor shutdown barrier over the customer engine. |
| `zeroship-workflow-server` | HTTP routes, enrollment/service authentication, configuration, readiness and manager lifecycle composition. |
| `zeroship-worker` | Existing executable hosting normal requests and the bounded workflow consumer with trusted creator context. |

```text
Normal Cargo dependencies; arrows are not network calls

workflow-server --> workflow-manager --> data-orm [platform binding]
       |
       +----------> workflow-client  --> authenticated metadata transport

worker ----------> workflow --------> data-orm [creator binding]
   |                   +-----------> storage / bundle
   +-------------> workflow-v8 -----> workflow + runtime
   +-------------> workflow-client

Control ---------> workflow-client
   +-------------> workflow-manager::deployments [authorized platform binding]

CLI -------------> workflow-manager [local platform metadata]
   +-------------> workflow         [normal app database]
   +-------------> workflow-v8

All workflow contracts use core / canonical typed identities
```

Manager and customer engine do not depend on each other. The client depends on
neither persistence implementation. Hosts inject metadata capabilities; local
composition can invoke native manager operations with a trusted local caller,
while production uses the client. Customer replay envelopes, inputs, payload types
and detailed execution errors remain in the customer library and V8 adapter.
Owner-specific errors are mapped to closed boundary failures rather than making
the manager depend on `WorkflowServiceError`.

The target keeps queue, scheduling, management and recovery as modules of the
manager. `zeroship-workflow-scheduler` is replaced, including its old binary and
configuration surface. The bundle crate continues to own artifact formats and
verification, not ORM deployment persistence. A generic deployment service or
additional ledger crate is outside this restructuring.

`WorkflowService` is a library handle, not a new deployable service. Its existing
scheduler methods do not justify retaining discovery in workers. Keep customer
transitions and bounded runner mechanics while replacing `WorkerTasks` local
polling and maintenance sweeps with delivered-job acceptance. Runtime-loader
interfaces should match actual I/O; constructing an already loaded runtime can
remain synchronous. `async-trait` is not an architectural requirement. Shipped
I/O stays on compio; no additional async runtime is introduced.

Update Cargo declarations, configuration registration, schema generation,
container build inputs, xtask selection and dependency gates with each move.
The existing client's `cyper` carrier follows the client crate; do not retain
duplicate clients or add Tokio as a normal dependency to avoid updating a gate.
Use main's shared ORM API and coordinate its changes with the ORM owner.

### Service operation inventory

This inventory defines responsibility and success semantics. Existing coordinator
routes are registered in
[`workflow-server/src/api.rs`](../../crates/zeroship-workflow-server/src/api.rs).
Queue and activation entries below are target operations; their exact wire
envelopes and endpoint registration must land with their producers and consumers.
An operation name here does not imply an available public HTTP route.

| Operation | Authorized caller and receiving owner | Successful result |
| --- | --- | --- |
| Enroll/replace instance | Deployment host and worker bootstrap to Control. | An instance identity bound to its enrolled key and authorized deployment context. |
| Register/drain instance | Enrolled worker to manager. | Recorded liveness/capacity; grants no app assignment by itself. |
| Resolve eligibility/place app | Trusted Control/host context to manager. | Durable app/worker revision admitted under capacity and zone constraints. |
| Poll/renew/release assignment | Enrolled worker to manager. | Only that worker's authorized scopes and current revision outcomes; release does not retire the app's recovery duty. |
| Register/activate deployment | Control to manager. | Idempotent immutable schedule metadata and monotonic activation state; dispatch readiness remains distinct. |
| Establish/close ingress scope | Trusted creator host through manager policy. | Durable recovery responsibility or an explicit fenced drain result. A worker cannot create authority for an arbitrary app. |
| Submit job/intents | Assigned worker or native manager scheduling logic to manager queue. | Receipt for the stable immutable specification; changed content under the same job identity conflicts. |
| Claim job | Enrolled worker with current assignment to manager queue. | A persisted delivery attempt and bounded authority, or no eligible work. |
| Heartbeat delivery | Its worker to manager queue. | Current bounded lease authority after fresh identity/placement checks; cannot revive a replaced attempt, expired execution or elapsed execution budget. |
| Settle delivery | Its worker to manager queue. | Atomic outcome, stable successors and scheduling/barrier changes, or replay of the matching receipt. |
| Submit/read management command | Creator-authorized Control to manager. | Durable command acceptance or closed delivery outcome; no customer history or result body. |
| Acquire/release deployment hold | Authorized queue or journal holder to Control. | Generation-fenced retention result for that holder class and app. |
| Start/signal/read run | App code or Rust caller through an authorized creator-bound handle. | Creator-side acceptance or detailed state. Customer bodies stay on this path. |

The worker requests work through outbound authenticated calls. Polling is bounded
and backs off when no eligible work exists; transport reconnection never changes
a logical operation's identity. The manager does not reach into V8 through a
private advance endpoint. Network loss changes delivery availability, not the
ownership of customer data or the durability of already accepted work.

Read/list operations return bounded pages with opaque, scope-bound cursors. A
cursor is a position, not an authorization token or a promise of a snapshot;
every page rechecks caller scope. Operations needing a stable scan explicitly
capture a revision/cutoff. Queue status remains scheduling metadata; detailed
workflow inspection goes through the authorized creator path.

## Configuration and local development

Host configuration parses once and produces validated Rust options. TOML and
programmatic construction configure the same underlying library behavior;
libraries do not independently reread environment variables. Use the normal
configuration precedence and secret handling rather than workflow-specific
fallbacks.

| Configuration owner | Relevant settings and boundary |
| --- | --- |
| Manager host | `WorkflowSettings` supplies listener, service peers, platform DB binding, body/page bounds and worker/assignment policy. `workflow.database_url` is a platform credential. |
| Native coordinator | `coordinator::Options::{worker_ttl, assignment_ttl, batch_limit, max_pending_management}` bounds placement and command behavior. |
| Native queue | `Options::{lease, transaction_timeout, max_successors, max_metadata_bytes}` bounds delivery and metadata transactions. |
| Metadata client | Client `Options::{timeout, max_request_bytes, max_response_bytes}` bounds the complete exchange. Each call uses the host signer. |
| Customer host | Normal creator DB/storage, trusted app identity, policy snapshot and execution limits. Existing `WorkerOptions` slot/execution limits remain relevant; worker maintenance scheduling settings disappear with their loops. |
| Scheduling/recovery host policy | Explicit misfire, overlap, reconciliation and capacity/backpressure bounds. New setting names are finalized with those modules, not invented CLI switches. |

Policy snapshots are host-owned and revisioned. Expired remote metadata does not
become self-renewing authority through a retry, a customer row or a mutable
`APP_ID` environment value. Runtime identity comes from the immutable trusted app
context. Enrollment, policy refresh, code availability and recovery eligibility
are separate readiness concerns.

```text
zeroship serve / Vite local host
       |
       +--> native manager + queue --> normal local platform metadata/catalog
       |
       +--> native consumer -------> normal app database + app storage
                   |
                   +--------------> same V8 executor and normal app bundle

               shared protocol, separate storage bindings
```

The CLI composes these libraries in process with normal app configuration. Local
calls can omit network and enrollment ceremony while preserving scope checks,
receipts, fences and recovery. The CLI owns setup/startup/shutdown; it contains no
bespoke cron evaluator, workflow bundle loader, deployment watcher or journal
scheduler. Supporting multiple app deployments in the CLI is a separate concern.

Customer history stays in the normal app database. The manager uses the normal
local platform metadata/catalog binding alongside deployment identities and
holds, not the customer binding. Do not add a dedicated workflow SQLite file,
workflow database environment variable or `--workflow-bundle`. Creators need not
supply `APP_ID`. Local co-location does not change the production database boundary.

Restart preserves queue metadata, journal receipts, pending intents and retained
bundles. Hot reload activates a new immutable deployment while existing generations
keep their pins. Local durability and failure behavior must match production;
replacing the queue with an in-memory shortcut would defeat that parity.

## Operations, security and backpressure

### Startup and readiness

The manager validates configuration, schema fingerprints, least-privilege grants,
authentication/replay storage and required host capabilities before admitting new
work. It recovers durable jobs, scheduling cursors and responsibility state from
its own DB. Startup does not scan customer databases or depend on a resident worker.
A health endpoint reports process liveness; readiness reports whether the host can
perform its required authenticated metadata operations.

The worker obtains its authorized runtime identity and creator context, enrolls,
registers and receives eligible assignments. It validates creator schema, policy,
bundle and payload capabilities before accepting ingress or execution. A workflow
whose artifact is unavailable remains observable and durable; it does not silently
run another deployment. Authentication or migration failure is a startup/readiness
failure, not permission to fall back to elevated credentials.

### Shutdown and crash recovery

Workers stop new claims and ingress, report draining, and finish bounded executions
or cancel them through the executor shutdown barrier. Quarantine closes isolate
admission; runtime shutdown must still join native operations before capacity is
reused or completion is published. Committed outcomes and
intents remain publishable even if shutdown cannot contact the manager. Unfinished
jobs may expire for redelivery, while the app's durable recovery duty remains.

Managers stop new admission, stop initiating capacity work, and allow owned
transactions to settle or become explicitly uncertain. Timers, job leases,
submission receipts and obligations live in storage, so another replica can
continue. Neither shutdown path deletes durable work merely to make a process
exit cleanly.

### Backup, restore and disaster recovery

Each database owner backs up its own state. Creator recovery includes journal
records and referenced payloads; platform recovery includes queue, schedules,
scope responsibility, enrollment, deployment metadata and holds. Retained bundles
must remain available for every recovered pin. Backup access does not give a
platform workflow process permission to read a creator database.

A process restart is different from restoring an older database snapshot. An old
snapshot can resurrect already settled jobs, lose receipts or reinstate old
assignment authority. At-least-once delivery and ordinary lease expiry alone do
not repair that loss. Independent database restores also cannot be treated as a
consistent snapshot of both zones.

Restore therefore closes admission and fences prior process/placement authority
before replay resumes. Operators must reconcile manager obligations and creator
receipts through authenticated worker jobs, validate payload and code availability,
and retain deployment holds while dependencies are uncertain. Never advance a
manager cursor or mark an app drained merely because restored metadata is empty.
If customer receipts were lost, external-effect duplication must be handled by
the destination's durable idempotency contract or explicit operator remediation.

The restore-epoch and reconciliation handshake is a required operational protocol
still to be defined and tested with the storage owners. Routine crash-recovery
tests are not evidence for snapshot-restore safety. No operator recovery path may
replace the private-zone boundary with direct Control access to creator storage.

### Backpressure and fairness

Bound request/response metadata, successor sets, claim batches, payload
preparation, live runs, replay/frontier size and worker slots through their owning
options. Placement capacity bounds assigned scopes; execution slots separately
bound simultaneous jobs. A worker claims only what it can start within valid
authority. Queue depth is not permission to overcommit the creator database.

Reject new unaccepted work before its durable customer commit when local admission
limits require it. After acceptance, queue or transport pressure leaves publication
pending; it cannot erase the run. Manager rejection must distinguish invalid
metadata, capacity pressure and temporary storage/authentication unavailability.

Scheduling and recovery process bounded pages and yield between app scopes.
Management and reconciliation need progress even while execution admission is
paused or saturated. Retry backoff and fair dispatch belong to manager/host policy;
a hot app or repeated broken deployment must not starve other eligible work.
Specific fairness and poisoned-job parking policy are still to be selected; silent
delete-on-retry-exhaustion is not an acceptable policy.

### Observability and trust limits

Correlate app, deployment, job, delivery attempt, assignment revision, run generation
and command identity where relevant. Emit closed reason codes for authentication,
capacity, stale fences, unavailable artifacts, publication backlog and recovery.
Measure queue age, due-work lag, retries, publication progress, capacity demand,
execution budget termination and hold/collection progress through native metrics.
Identifiers belong in appropriately scoped traces; avoid unbounded metric labels.

Platform logs and metrics contain no customer inputs, outputs, history, signals,
raw database diagnostics, credentials, signed assertions or remote response bodies.
Detailed workflow errors stay in creator storage and authorized worker views.
Metering remains trusted runtime infrastructure, never a customer-supplied result.

Remote service transport uses authenticated TLS and validates the intended
endpoint and audience. The native client accepts plaintext only for literal
loopback development origins, rejects redirects, bounds streamed responses and
returns closed failures. Body closure and response identity checks apply to nested
metadata as well as top-level envelopes.

Crate boundaries make authority reviewable; database grants, network isolation,
trusted runtime bindings and transaction predicates enforce it. The protocol
defends against stale/foreign requests and malicious app selectors. It cannot
make a compromised process with creator DB credentials unable to damage that
same creator's data. Such a process still must not gain Control credentials,
other apps' contexts or a way to forge platform retention and usage authority.

## Failure behavior and verification

| Failure or race | Required behavior |
| --- | --- |
| Input upload or acceptance transaction fails | Return failure without an accepted run; retain collectible preparation state where needed. |
| Acceptance commits but enqueue or response is lost | Request receipt preserves the run; its intent and pre-existing scope duty recover publication. |
| Worker remains healthy while publication repeatedly fails | Manager reconciliation deadline still produces recovery work. |
| Last worker disappears before publication | Durable scope duty requests authorized capacity and dispatches reconciliation. |
| Manager crashes after submitting a job but before replying | Same job identity returns the stored submission result. |
| Worker crashes before customer commit | Redelivery reclaims the frontier without assuming a result exists. |
| Customer COMMIT is uncertain | Read the creator receipt after settlement; never infer rollback from timeout. |
| Worker commits then loses ACK | Redelivery reads its customer outcome without executing the committed turn again. |
| ACK and outbox both publish successors | Shared immutable IDs deduplicate; changed successor content conflicts atomically. |
| Manager COMMIT is uncertain | Retry exact settlement; the stored receipt determines whether it committed. |
| Placement changes or enrollment is revoked during a lock wait | Fresh authorization rejects new mutation; database clock and original budget remain binding. |
| Old heartbeat reply is lost | Retry can observe/renew a still-live stored lease; it cannot revive locally expired execution or restore elapsed budget. |
| Old delivery tries to settle a replacement | Attempt and assignment fences reject it; settled-receipt replay admits no new writes. |
| Concurrent cron replicas or delayed activation | Occurrence/cursor transaction and activation revision prevent duplicate or retargeted work. |
| Signal races timer, restart or child completion | Creator wait/generation predicates select the valid transition and preserve durable events. |
| Pause is rejected or an old command ACK arrives | Only the matching provisional barrier changes; newer lifecycle authority survives. |
| Payload promotion races collection | Serialized eligibility prevents deletion of committed references. |
| Artifact reclamation races hold acquisition | Deployment fence decides admission; pending jobs/history retain their distinct holds. |
| Capacity adapter or creator DB is unavailable | Demand and accepted jobs remain durable; no cross-zone database fallback occurs. |
| A database is restored to an older snapshot | Fence prior authority and reconcile both owners' durable state before reopening admission; missing receipts require explicit duplicate-effect handling. |

Native Rust tests exercise behavior through the owning crate, with PostgreSQL and
file-backed SQLite for ORM contracts. Required external environments belong to
Testcontainers using major image tags. Unavailable required infrastructure fails
tests; it does not turn them into optional checks. Examples own their Vitest,
TypeScript fixtures and Playwright suites. Use the workflow xtask/native test
selection rather than adding bash orchestration or source-text gates.

| Test owner | Required evidence |
| --- | --- |
| Core/identity | Closed nested variants, malformed IDs/revisions, stable wire identity and no customer fields. |
| Manager | Concurrent capacity/claims, sparse recovery pages, atomic command/queue changes, stable receipts and storage IDs, authority after lock waits, shortened budgets and uncertain-commit retry. |
| Scheduling | Calendar/DST parity, catch-up and overlap semantics, activation without a resident worker, schedule replacement/disable, stable occurrences and due-work recovery. |
| Customer engine | Acceptance/outbox atomicity, duplicate delivered jobs, stale frontiers, receipt retention, policy expiry, signal/child races and generation-safe restart. |
| Worker/V8 | Pinned complete module graph, immutable app context, bounded synchronous and async execution, stopped-native-work barrier and a consumer with local scheduling removed. |
| Client/server | Real authenticated HTTP, instance-versus-role keys, replay protection, enrollment changes during waits, scope/order validation, TLS, streamed bounds and reuse after cancellation. |
| Retention/payloads | Upload failure, corrupt reads, uncertain reference promotion, concurrent collection, queue-versus-journal holders and delayed generation messages. |
| Host integration | Private disjoint DB access, no forbidden grants, zero-worker capacity recovery, ongoing-heartbeat publication failure and restart across both commit boundaries. |
| Local/examples | Same protocol and durability with local native composition; each example's creator-facing behavior through its own tests. |

Lock/cancellation tests observe the actual blocked operation and its storage
result rather than sleep and assume progress. Preserve the distinction between
rollback before COMMIT and uncertainty after terminal dispatch. Dependency gates
inspect normal Cargo edges so production workers cannot import manager/server
persistence, platform services cannot reach the customer engine/V8, and clients
cannot reach either ORM store. Test fixtures may compose both zones.

## Implementation progress and remaining decisions

### Implemented foundations and current gaps

The branch contains the customer-bound ORM journal, replay/lifecycle operations,
durable creator management receipts, payload preparation, bounded runner, V8
binding/executor, normal verified bundle loading and deployment-hold foundations.
The local host currently composes that journal and runner.

The crate split includes the metadata client, closed job/delivery contracts,
manager ORM queue and platform deployment ledger. Native coordinator placement
and management now share the queue's ORM namespace and transaction handle. Initial
native coordinator/queue regressions passed, while concurrent first-registration
coverage exposed a PostgreSQL conflict through a redundant unique identity. The
canonical primary-key correction and HTTP server integration are being verified.
Storage tests do not establish a completed distributed workflow system.

Manager cron/timer discovery, durable scope deadlines, capacity activation,
creator job receipts/publication intents, queue HTTP delivery and the simple
worker consumer still require implementation and integration. Existing customer
scheduler/task polling and maintenance code remains a foundation to replace.
Its existence does not satisfy manager-owned scheduling.

Legacy production paths still include Control journal access, worker platform
queries, grants incompatible with private zones and Control/Gateway workflow
advancement. Remove their producers, consumers, schema/grant dependencies and
configuration together at cutover. Do not describe the production boundary as
complete while those paths remain. There are no production users requiring
compatibility aliases or parallel legacy modes.

### Decisions still requiring an explicit contract

| Decision | Fixed requirement and remaining choice |
| --- | --- |
| Enrollment bootstrap and revocation | A revoked worker cannot regain equivalent authority by automatic enrollment. Finalize bootstrap trust, replacement authorization and registry freshness with auth ownership. |
| Placement eligibility and capacity provider | Only platform-authorized app/zone combinations may be assigned. Select the trusted eligibility source and host adapter's durable request/progress contract. |
| Cross-zone lease transfer | Queue and creator clocks are independent. Define remaining-authority transfer or an explicit skew bound and conservative conversion; absolute timestamps alone do not establish shared-clock safety. |
| Complete job envelopes | Keep closed metadata. Finalize activation, event/fanout, continuation cursors, management lifecycle revisions and operation-specific deployment prerequisites before their consumers are wired. |
| Scope retirement | Define ingress epoch closure and durable drain evidence. Registration expiry and empty polling cannot retire unpublished-work responsibility. |
| Receipt retirement | Define admissibility fences and publication/settlement watermarks before deleting job deduplication state. Retain it until that proof exists. |
| Schedule evolution | Finalize timezone-data interpretation changes and cancellation of queued, unaccepted occurrences on disable. Preserve existing declared policy semantics unless deliberately changed. |
| Dispatch fairness and persistent failure | Define fair progress and observable parking/retry policy without deleting accepted work or starving management/reconciliation. |
| Snapshot restore | Define restore epochs, fenced admission and cross-owner reconciliation with the storage owners; process-restart recovery alone cannot protect lost receipts or resurrected authority. |

These are design decisions, not unspecified permission to improvise in separate
implementations. They do not reopen the database boundary or require another
broker/service. Mid-run upgrade has its own unresolved semantic contract and is
outside the queue cutover.

### Dependency-ordered completion

- Finish native server composition and schema/grant verification with the shared
  manager transaction domain and current enrollment checks.
- Finalize the missing closed delivery, scope-recovery and retention contracts;
  add their manager models using the canonical migration/ORM pipeline.
- Connect normal deployment registration, activation and queue holds to manager
  scheduling; move calendar ownership and remove the standalone scheduler host.
- Add creator delivered-job acceptance, receipts and publication intents together
  with durable scope responsibility before enabling new ingress semantics.
- Compose the bounded worker consumer, payload/retention jobs and trusted runtime
  loader; remove worker schedule discovery and independent maintenance loops.
- Route signals, dependent work and management through the common protocol;
  integrate the capacity adapter and prove progress without a resident worker.
- Embed the same manager/consumer in the thin local host, then remove legacy
  Control/Gateway advancement, cross-zone SQL/grants and obsolete settings.
- Run owning native, authenticated host, private-zone and example suites before
  reporting the production cutover complete; integrate shared ORM changes from
  their owner rather than introducing workflow-specific replacements.
