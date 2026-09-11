# Workflow server and shared local execution

**Status:** Final design; implementation in progress. This is the implementation
target agreed during the workflow architecture review. It supersedes the older
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
The [workflow reference](../reference/workflows.md) describes the current code;
this document describes the replacement.

The Rust app-operation API now uses typed requests, responses and domain errors.
Local and Control paths share input validation and restart safety rules. Local
mutations and checkpoint batches are transactional, and acceptance returns
before execution. The service-owned schema, shared store implementation,
workflow server and worker polling remain to be implemented; the current runtime
still uses Control and the local mini-engine.

## Decision

Deploy `zeroship-workflow-server` as the authority for workflow operations,
journals, scheduling and task leases. The existing worker fleet executes app
code by polling that server. Workflow state and its runnable frontier live in
the same PostgreSQL transaction domain. The server owns their writes.

Keep `zeroship-workflow` as the Rust engine and service library, independent of
V8. Keep `zeroship-workflow-v8` as the app binding and executor adapter. Replace
the unfinished `zeroship-workflow-scheduler` host with the workflow server;
scheduling becomes an engine responsibility rather than a separate deployment.

Local development embeds the same service and task runner in `zeroship serve`,
using SQLite and direct Rust calls. Local execution has the same workflow
semantics as deployed execution, including children and compensation.

Retain the bounded inline `step.run()` model. A workflow advance executes its
ready callbacks in the worker and submits their outcomes for persistence.
Separately scheduled Activities, arbitrary closure serialization, user-managed
task queues and Temporal protocol compatibility are outside this design.

## Ownership and deployment

```text
Creator tools --> Control
                  app/deploy/plan authority
                       |
                       | authorized management and deploy notifications
                       v
App code --> workflow-v8 --> app-scoped Rust client
                                       |
                                       v
                         +-----------------------------+
                         | Workflow server             |
                         |                             |
                         | authorize and admit         |
                         | journal and lifecycle       |
                         | schedules and signals       |
                         | task leases and completion  |
                         +--------------+--------------+
                                        |
                                        v
                              PostgreSQL workflow schema
                              + workflow payload storage

                         Workflow server
                                ^
                                | poll / heartbeat / complete
                                |
                         Existing worker fleet
                         pinned app bundle -> V8 execution
```

| Component | Responsibility |
| --- | --- |
| Control | Own app lifecycle, deploy selection, plans and creator authorization; submit management operations and schedule reconciliation notifications. |
| Workflow server | Own all workflow state changes, admission, durable timers, signals, scheduling, leases, output access and retention. Execute no creator code. |
| Worker | Poll when capacity is available; load the assigned deploy; replay and execute; return outcomes under the assignment's lease. |
| Gateway | Route app requests and the explicit public signal ingress to the appropriate service. Background workflow execution bypasses Gateway. |
| Migration tooling | Create workflow tables and grant runtime DML privileges. Runtime processes verify schema readiness and hold no workflow DDL authority. |

The workflow server can have replicas sharing the workflow database. Claims and
admission are coordinated in database transactions. In-memory timers,
notifications and worker cache hints are accelerators; restarting any process
must not erase the ability to discover work.

The initial deployed placement uses the existing platform PostgreSQL database
with a dedicated `workflow` schema. Creator business databases remain separate
from workflow storage. Sharing or moving a creator database neither shares nor
moves workflow identity. Workflow exports, deletion and backups are the workflow
service's responsibility. This deliberately replaces the older requirement to
keep the execution journal inside each creator database.

## Rust composition

The following are target types, not interfaces that already exist:

| Type or package | Contract |
| --- | --- |
| `WorkflowService` in `zeroship-workflow` | Typed app operations, lifecycle transitions, scheduler and completion logic over persistence and host ports. |
| `AppWorkflows` | A handle bound by a trusted host to an app and its authorization context. Its methods cannot select another app. |
| `WorkflowClient` | Remote implementation of app operations; serialization and transport errors terminate at this adapter. |
| `WorkflowExecutor` | Execute a typed assignment and return typed outcomes. V8 implementation belongs to `zeroship-workflow-v8`. |
| `WorkflowStore` | Transactional storage contract implemented by PostgreSQL and SQLite. Includes start, signal, lifecycle, claim and apply, rather than only apply. |
| `zeroship-workflow-server` | Service authentication, HTTP adapters, PostgreSQL composition, payload storage and process lifecycle. No V8 dependency. |

Domain requests, states and errors are Rust types. Workflow input and output may
be JSON values, but ordinary Rust operations do not construct HTTP request bodies
or interpret status codes. A trusted Rust platform component may use the remote
client; an embedded host uses the service directly. Rust integration here means
starting and controlling creator workflows; arbitrary native Rust workflow
definitions are outside this design.

The JS journal interpreter has a single implementation consumed by the SDK,
bootstrap and runtime embedding. Local and deployed execution use that same
interpreter. Transport, persistence and app-policy providers vary by host;
workflow behavior does not.

## App isolation and trust

Authorization is app-level. The service does not implement end-user business
permissions; app handlers decide whether an end user may start or signal their
app's workflows.

- The binding receives the app identity from trusted runtime composition.
  Creator arguments cannot override it. A run handle carries the same scope.
- Production app clients present a short-lived, app-scoped capability issued by
  Control for the workflow service. It binds audience, app and allowed operations.
  The worker obtains capabilities through authenticated host bootstrap/refresh;
  it does not mint them from a Control master key. Expired or unavailable
  authority refuses new operations.
- Control management requests carry authenticated service identity and the
  authorized app/actor context. This surface includes management permission
  checks and audit provenance; it is inaccessible to creator JS.
- Worker polling uses a worker service identity. The server selects tasks; a
  worker poll cannot request an arbitrary app/run pair. Each assignment carries
  an opaque task token bound to worker identity, app, run, generation and lease.
- Task tokens, signing material, service credentials and raw storage credentials
  stay in Rust. The V8 envelope contains execution data, not task authority.
- Every app operation, signal, child edge and output reference is checked against
  the authenticated app. Cross-app lookups return the same not-found response
  as absent objects. Nested commands cannot nominate a different app.
- App identity participates in keys, joins and foreign keys. The service's app
  handle binds SQL scope; parameterized predicates and relational constraints
  protect reads, mutations, child links and payload references.

`zeroship_workflow_owner` becomes a migration-only owner. The workflow server's
runtime role receives the required DML grants. Worker, Gateway and app database
roles have no direct workflow-table privileges or workflow-owner membership.
The worker's existing access to business data is a separate subsystem; this
change removes its workflow database authority rather than claiming to remove
every privilege from the worker process.

The workflow server is a trusted multi-app service. App-scoped handles protect
creator boundaries but do not contain a compromise of that server. Likewise,
worker isolation depends on the runtime boundary; a process compromise can
expose credentials and tasks present in that process. A native method or a
namespace name is not a process security boundary.

## Service contract

Use typed HTTP requests over the existing compio-compatible transport. App
operations and worker task operations have separate authorization rules even
though they share a server. Adopt the existing service authentication machinery
and register explicit workflow endpoint audiences. There is no unsigned mode.

These are target routes, not current endpoints:

| Surface | Operations |
| --- | --- |
| `/v1/apps/{appId}/workflows/{name}/runs` | Start a run or a typed batch. |
| `/v1/apps/{appId}/workflow-runs` | List runs within the authenticated app. |
| `/v1/apps/{appId}/workflow-runs/{runId}` | Read status and perform typed lifecycle operations. |
| Run subresources `signals`, `output`, `steps` | Deliver a signal and read scoped output/checkpoints. |
| App subresources `workflow-topics`, `workflow-signal-tokens` | Broadcast and issue narrowly scoped ingress capabilities. |
| `/v1/tasks/poll` | Bounded long poll for an assignment. |
| `/v1/tasks/{taskId}/heartbeat` | Renew a current assignment and receive control intent. |
| `/v1/tasks/{taskId}/complete` | Submit outcomes, including workflow failures, for validation and commit. |
| `/v1/tasks/{taskId}/release` | Relinquish an assignment after execution has stopped, or report an execution infrastructure failure. |

Request DTOs in the core crate are the wire authority. Lifecycle operations use
an enum rather than unvalidated operation strings. Paths are routing data, not
authorization proof. There are no header aliases or compatibility routes.

Creator tools keep using the authenticated Control surface; Control calls the
workflow client. The public run/topic signal routes can remain on the app
origin, with Gateway forwarding to the workflow server. The workflow server
verifies capability audience, app, run/topic, allowed signal types, expiry and
revocation epoch before writing a signal. Gateway holds no workflow signing key.
Scheduled maintenance is invoked by the service loop, not an exposed tick API.

Control's current `/internal/workflows/*` implementation and the
Gateway-to-worker workflow advance routes disappear at cutover. The new server
is the explicit process boundary for necessary workflow communication.

## Persistence and atomicity

The workflow schema holds runs/generations, ordered journal records, pending
waits/signals, subscriptions, schedules, broadcasts and delivery progress,
output references, task completion receipts and admission state. These are
conceptual records; the implementation should combine tables where their
transaction and lifecycle contracts coincide.

A run's state contains its deploy pin, generation, current lease, control
intent and next eligible wake-up. An indexed due frontier is sufficient for
task discovery. There is no independently committed scheduler timer store,
registration acknowledgement or journal-to-scheduler reconciliation protocol.

All transactions acquire app admission/lifecycle locks before run locks. A
transition touching related runs locks the full run set in canonical ID order.
The frontier is recomputed from current waits, signals and child outcomes while
holding those locks. An old replay snapshot cannot overwrite a newly delivered
signal's wake-up.

| Transition | Changes committed together |
| --- | --- |
| Start | Idempotency resolution, deploy pin, input, run and due frontier. |
| Scheduled start | Scheduled occurrence identity, new run and next schedule occurrence. |
| Signal delivery | Deduplicated mailbox record and resulting run frontier. |
| Claim | App admission reservation, fresh task identity, lease ownership and deadline. |
| Completion | Accepted outcomes, journal records, consumed signals, run state, child/parent effects, next frontier, lease release and completion receipt. |
| Continue as new | Current generation completion, successor input/reference, selected deploy pin and runnable successor. |
| Lease recovery | Fence the abandoned task, release its reservation and expose the unfinished run for another claim. |

Broadcast ingestion commits a deduplicated broadcast record. Fan-out works in
bounded transactions: each commits recipient delivery, wake-up and progress.
Unique delivery identity makes restarting fan-out safe. This is asynchronous
fan-out inside the workflow store, not an atomic fleet-wide transaction.

Concurrent duplicate starts obey the existing `join`, `reject` and `replace`
policies under database uniqueness and locks. A live-run key is not a permanent
HTTP retry receipt: once that run is terminal, a later start may reuse the key.
Transport retries additionally carry a request identity and body digest; a
retained receipt returns the original result even if the run has completed.
Reusing a request identity with another body is a conflict. Its retention is a
documented retry window. The client never automatically retries an ambiguous
mutation with a new request identity. A start without a caller key remains a
distinct business request, even though its transport retries are deduplicated.

## Picking up and completing work

```text
Worker                            Workflow server                 Store
  |                                      |                          |
  | reserve local execution capacity     |                          |
  |-- long poll ------------------------>|                          |
  |                                      |-- atomic claim --------->|
  |                                      |<-- committed lease ------|
  |<-- task + pin + journal + token ------|                          |
  |                                      |                          |
  | load immutable deploy snapshot       |                          |
  | enter app isolate, replay, execute   |                          |
  |-- heartbeat ------------------------>|                          |
  |<-- renewed deadline/control intent --|                          |
  |                                      |                          |
  |-- outcomes + task token ------------>|                          |
  |                                      | validate assignment       |
  |                                      |-- atomic completion ---->|
  |                                      |<-- committed receipt ----|
  |<-- accepted -------------------------|                          |
  | release local capacity               |                          |
```

Workers report available execution capacity and compatible runtime capabilities.
Deploy affinity is a cache hint; it does not confer ownership or authorize app
access. A worker can cold-load the pinned bundle without belonging to the app's
HTTP routing ring. Its HTTP handler and workflow runner share bounded runtime
capacity, with reserved admission preventing either workload from starving the
other.

The server uses the database clock for due times and leases. A claim carries a
fresh unpredictable token and monotonic fencing epoch. Heartbeats and completions
must match the current app, run, generation, worker and epoch. A failed renewal
stops execution and prevents submission under presumed ownership. Transient
failure does not extend the last confirmed deadline.

Completion validates command shapes, ordering, declared workflow targets, output
references and configured limits. The server rechecks the live lease inside the
transaction before applying results. A lost poll response is recovered by lease
expiry. A lost completion response is retried using the same task and digest;
an already accepted completion returns its stored receipt without applying
again. A stale task that never committed is rejected even if its outputs look
valid. The server does not hold a database transaction while V8 or network I/O
executes.

Shutdown stops new polls and drains active work. A worker may release a task only
after stopping its execution; otherwise it lets the lease expire. Lease recovery
is a normal scheduling path. Process-local counters are not authoritative for
app-wide limits.

## Step execution and replay

Workflow orchestration is deterministic between durable operations. The
interpreter rejects unjournaled nondeterminism, nested durable operations and
incompatible replay commands. Journal records preserve the observations replay
needs: operation identity and order, outcomes, wait resolution, child results,
retry decisions and compensation progress. Persisting output values alone is
not the replay contract.

`step.run()` executes a bounded callback in the assigned worker. Completed
records replay as saved values. A missing record executes the callback. The
worker returns when the ready frontier completes or reaches a durable wait; the
server validates and commits the frontier. Concurrent callbacks have stable
identities and journal order, while their external effects may interleave.
The configured frontier commits atomically; a crash before commit can repeat
callbacks whose effects already happened.

The server owns durable retry attempts and backoff. Retry policy is validated
against app limits and fixed for the recorded operation. Worker heartbeats
renew task ownership, not a callback's permitted execution duration. Callback
timeouts and dispatch limits are enforced by the executor. A timeout must stop
or quarantine the execution and cancel outstanding host operations before its
capacity is reused; rejecting a Promise while leaving its callback running is
not sufficient. Cancellation of an external request cannot undo a side effect
already accepted by another system.

The side-effect idempotency key is derived from app, run, generation, operation
identity and effect phase. It stays stable across retries and worker handoffs;
the attempt counter is separate metadata. An intentional restart creates a new
generation for re-executed effects. Compensation has its own stable effect key.
The SDK provides the key to effect code; integrations must actually pass it to
systems supporting idempotency. The workflow server cannot make arbitrary
external effects exactly once.

Long-lived workflows release workers while sleeping or waiting for signals.
A long external job is submitted by a bounded step and completed through a
correlated, idempotent signal. This design does not add a separately scheduled
Activity API. Adopting Activities later would be an explicit product and wire
contract change with its own executor design, not serializing inline closures.

## Triggers and controls

| Source | Server behavior |
| --- | --- |
| App or platform `start()` | Authenticate app, select and validate deploy, commit runnable run, return handle. Worker pickup is asynchronous. |
| Cron or fixed interval | Apply overlap/catch-up policy and commit occurrence plus runnable run. Calendar schedules keep timezone semantics; intervals follow elapsed duration. |
| Child call or batch | Commit parent checkpoint/wait and child runs; children remain in the parent's app and inherit its deploy snapshot. |
| Continue as new | Complete the current generation and create a fresh run using the active deploy. Pending compensation obligations prevent continuation. |
| Timer or retry deadline | Make the recorded frontier eligible when due. |
| Direct or topic signal | Deduplicate and buffer the signal; resolve matching waits and make their runs eligible. |
| Child completion | Commit child result and parent notification/frontier together. |
| Worker loss | Fence expired ownership and expose unfinished work for retry. |

Pause stops new forward work. An already leased frontier may settle and record
its outputs, but completion observes the pause and leaves the run parked.
Resume recomputes the frontier; it does not blindly execute a still-pending wait.
Cancel is cooperative at checkpoint boundaries. A valid in-flight result is
recorded before the server enters cancellation/compensation, preserving the
information needed to compensate completed effects. Signals and administrative
intent are durable even while execution is leased.

Restart requires quiescent execution: a live lease, unsettled descendants or
active compensation returns a conflict. The caller can cancel and wait first.
Restart retains the run ID, advances an internal generation and retains only the
explicitly selected replay prefix. Retained records stay immutable. Re-executed
operations receive new effect identity. Switching to the active deploy requires
a full restart; a partial replay prefix stays on its original deploy. Ordinary
deployment never changes the code of a live run.

Compensation walks completed compensable operations in reverse journal order.
Each compensator is a bounded callback executed by a worker, with the same
lease, retry and checkpoint rules as forward execution. Partial compensation
failure remains visible in run status; it is not reported as successful rollback.

## Platform policy and deploy lifecycle

Control remains the authority for app archive state, plan eligibility, spending
restrictions, current deploy and operator workflow switches. The initial server
receives narrow read privileges for that authority in the shared PostgreSQL
database. Start, claim and schedule reconciliation check current policy in their
transactions, using the existing app lifecycle locking protocol so an archive
cannot race an admitted claim. This introduces no speculative asynchronously
cached policy authority.

Disabling admission prevents new work. Active frontiers follow the documented
drain/cancellation policy and remain bounded by their existing budgets. Status
and necessary management operations remain available to authorized callers.
This policy is enforced by the server, so bypassing Gateway cannot bypass it.

Control records a durable deploy notification with activation. Retried delivery
causes the server to reconcile schedules against the current authoritative
deploy, not blindly apply the notification's older manifest. The server also
reconciles at startup and periodically; this is recovery of deployment input,
not a second copy of workflow timer state. Reconciliation does not recreate
missed pre-activation schedule occurrences or duplicate recorded occurrences.

Runs hold durable references to immutable deploy snapshots. Control's bundle GC
must consult workflow retention authority and keep referenced bundles. Pinning
and deletion serialize on deploy retention ownership in the shared database;
an unreachable workflow authority makes deletion refuse, never assume empty.
Closed runs retain their pins while restart/history policy requires them.

## Payloads, retention and metering

Large values are stored as workflow-owned payloads. The server owns blob storage
credentials. Worker Rust streams reads and staged uploads through authenticated
app/task-scoped operations; creator JS receives output handles, never a store URL
or a service credential. References are scoped to the app/run and verified
against the task before being admitted into the journal.

Upload completion precedes journal reference commit. Staged uploads have durable
identities and an expiry; the completion transaction promotes a valid staged
object to a referenced object. GC serializes against that promotion and rechecks
references before deletion. An abandoned upload can be collected without
deleting a live result. Retention follows child/parent dependencies, replay
generations and deploy pins. A lease failure cannot authorize deleting another
app's payload.

The server emits usage for starts, signals, durable transitions and retained
workflow storage; workers emit actual execution resource usage, including failed
attempts. Events have stable identities so retries of a completion or outbox
delivery do not charge the same accepted transition again. Business idempotency
does not suppress metering real repeated execution. The server uses the existing
metering integration, with durable event intents committed alongside the state
changes they describe.

## Local development

```text
zeroship serve
|
+-- HTTP app runtime
|       |
|       +-- env.workflows --> AppWorkflows
|                                  |
+-- embedded WorkflowService <-----+
|       |
|       +-- same transitions, scheduler, leases and retries
|       +-- SQLite store: .zeroship/workflows.sqlite
|       +-- local workflow payload files
|       +-- local app/deploy policy provider
|
+-- embedded worker runner
        |
        +-- direct Rust task calls to WorkflowService
        +-- same V8 executor and journal interpreter
        +-- immutable local deploy snapshots
```

`zeroship serve` starts the embedded service and worker runner automatically.
An ordinary local run needs no workflow daemon, PostgreSQL, container runtime,
Control instance or network service credentials. Embedding is constructed by
the CLI; it is not an authentication-bypass mode in the deployed server.

Local app identity comes from trusted project resolution. For an undeployed
project the CLI persists a generated typed app identity under `.zeroship`.
Creators do not supply an `APP_ID` environment variable. Separate project roots
have separate default storage and identity. All in-process handles still carry
app scope, so the isolation contract can be exercised locally.

SQLite implements the same transaction and state-transition contract as
PostgreSQL, using serialization appropriate to SQLite rather than emulating
PostgreSQL SQL. Local time and task transport are host adapters. The engine
does not have a separate local fold, lifecycle implementation or unsupported
children/compensation branch. Schedule, signal, child, compensation, restart,
output and continuation behavior must pass the shared contract suite.

The CLI initializes the local workflow schema through the migration tooling.
PostgreSQL provisioning and SQLite initialization use the same owned schema
definition and supported dialect emission. The runtime engine does not grow a
second schema emitter or provision tables while claiming a run.

Stopping the CLI preserves journals, payloads and executable snapshots. On the
next start, the shared scheduler discovers due work and recovers expired leases.
The in-process runner follows the same claim/heartbeat/complete protocol through
typed calls, including completion receipts, without making a loopback HTTP call.

### Code changes and hot reload

Each runnable local workflow deployment is an immutable executable snapshot of
the workflow module and its dependencies. The dev host saves the compiled
snapshot and its content identity before accepting runs pinned to it. A live
module URL served by Vite is not a deploy pin. Workflow execution must not fetch
changed source through an old snapshot's imports.

Hot reload installs a new local active snapshot. New runs and reconciled
schedules use it; existing runs and their children use their saved snapshot.
Continue-as-new and an explicit full restart can select the new snapshot.
The local payload/retention adapter keeps snapshots while runs reference them.
Missing snapshots park affected runs with an actionable error rather than
replaying against current source.

An explicit local reset removes workflow state, workflow payloads and unneeded
workflow snapshots. It does not remove business databases, KV or storage data.
Code changes and schema incompatibility never silently reset persisted runs.
The CLI reports incompatible workflow-store schema and directs the developer to
the explicit reset operation; no legacy mini-engine fallback is constructed.

### Local signals and policy

App-issued signals and topic broadcasts use the embedded service. The local
HTTP host also exposes the same run/topic signal ingress semantics using
locally issued capability tokens. Reaching that loopback host from an external
provider requires an explicit developer-configured tunnel. Production keys are
not imported, and local tokens are never valid against the deployed server.

The local policy provider supplies explicit development limits and deploy
metadata. It does not pretend to reproduce live billing or organization
authorization. Pure engine tests use a controlled clock to advance sleeps and
retry deadlines; normal local execution uses real time. These are the intended
environment differences, rather than missing workflow features.

## Configuration and operations

Use the repository's generated configuration system and secret providers.
Programmatic Rust construction and TOML resolve into the same validated settings
types. Credentials use existing secret handling, never plaintext examples or
ad hoc environment parsing.

| Host | Configuration responsibilities |
| --- | --- |
| Workflow server | Listen address, service identity, workflow database secret, payload storage, task lease policy, poll bounds, admission, retention and observability. |
| Worker | Workflow server address, worker identity, task slots, heartbeat/poll policy and pinned-deploy cache. No workflow database URL or Control token-minting material. |
| Control | Workflow server address, management identity and reliable deploy notification delivery. |
| Local CLI | Project-local workflow state/payload paths, executable snapshots and development limits. |

Configuration validates heartbeat/lease and execution-budget relationships so
the executor can stop before unconfirmed authority expires. Startup verifies
the store schema and required credentials. Readiness depends on persistence and
the ability to authenticate work. A transient bundle, payload or policy outage
produces explicit backpressure; it does not discard accepted runs.

Expose pending-work age, expired leases, replay failures, rejected stale results,
app admission pressure, payload failures and retention health through the
existing metrics/logging stack. Use app, run, task and deploy identities for
correlation. Do not put secrets or user payloads in diagnostics.

## Implementation and verification

Implementation proceeds in reviewable commits, retaining the current runtime
until the replacement passes its contracts. The temporary coexistence is source
development only; a deployment has a single workflow writer and protocol.

- Extract typed app operations, policy/clock/executor ports and shared replay
  behavior. Expand the store contract so local lifecycle and scheduling no
  longer duplicate deployed semantics.
- Implement the service-owned PostgreSQL schema and SQLite adapter together.
  Establish transaction, ownership, retry and completion-receipt invariants
  before wiring HTTP or V8 execution.
- Replace the scheduler host with `zeroship-workflow-server`, including service
  authentication, scoped clients, task polling and operational configuration.
- Move the worker onto assignments and remove workflow DDL/DML credentials,
  Control token minting for workflow calls and direct workflow payload access.
- Compose the shared engine in the CLI, including immutable local snapshots and
  complete feature parity. Integrate the existing V8 replay implementation
  through a single executor path.
- Wire Control deploy/policy/retention integration and Gateway signal forwarding.
  Move workflow maintenance into the server. Update SDK clients and examples
  with the new contract.
- Cut over producers and consumers together. Remove Control workflow SQL,
  scheduler registration state, workflow advance routes, unsigned flags,
per-app journal provisioning, the dev mini-engine and obsolete configuration.
  Development fixtures are recreated explicitly; there is no legacy protocol
  adapter, dual-write mode or production-history migration promise.

Required behavior is ordinary Rust testing, with PostgreSQL and other external
test services owned by Testcontainers using major image tags. Examples retain
their own Vitest/Playwright fixtures and tests. `cargo xtask test workflow`
remains the aggregate entry point and must include local and deployed coverage.

| Contract | Required evidence |
| --- | --- |
| App isolation | Another app cannot start under its identity, inspect/list/mutate a run, attach a child, deliver a scoped signal or read/promote a payload. Forged scope and task completion are rejected. |
| Database authority | Real worker, Gateway and app roles cannot read/write workflow tables or assume the workflow owner; service runtime cannot run workflow DDL. |
| Durable acceptance | Concurrent keyed starts, scheduled occurrences and transport retries create only the intended runs, including retry after terminal completion. |
| Lease fencing | Worker/server death, lost poll response, lost completion response, stale heartbeat and delayed old completion preserve progress and reject stale writes. |
| Wake-up races | Signals, child completion, pause/resume and cancellation racing task completion cannot erase a wake-up or bypass the requested lifecycle state. |
| Replay and effects | Replays return committed values; incompatible history is rejected; retry keys remain stable; uncommitted external effects may repeat and are tested explicitly. |
| Bounded execution | Timeout and lost authority stop/quarantine callbacks and host operations before capacity reuse; HTTP and workflow workloads obey configured admission. |
| Local parity | Shared scenarios pass with SQLite/direct calls and PostgreSQL/network calls, including children, compensation, schedules, restart, continuation and outputs. |
| Local persistence | CLI restart resumes saved work; code edits cannot change a pinned run; missing snapshots park safely; reset stays within workflow-owned data. |
| Deploy and retention | Activation notification loss/reorder, archive races, pinned old bundles and staged-output GC cannot lose runnable work or delete referenced data. |
| Topology | Real server and workers execute without Gateway workflow dispatch, worker journal access or Control workflow SQL. Required service outages fail tests rather than skip cases. |

The refactor is complete when these contracts are implemented and verified, not
when package names or HTTP routes have been moved.

## Relationship to Temporal

The server/worker ownership and polling model follows the same broad shape as
[Temporal's architecture](https://docs.temporal.io/encyclopedia/architecture/how-temporal-works).
The bounded inline step is closer to a
[Local Activity](https://docs.temporal.io/local-activity) than an independently
scheduled Activity, without claiming API or execution equivalence.

We choose the native engine to preserve direct control of the V8 executor,
compio runtime, immutable app bundles and app-scoped primitives. This accepts
responsibility for durable execution correctness and operational tooling; it
does not claim Temporal's maturity or a measured cost advantage. The decision
does not import Temporal's full feature surface into this refactor.
