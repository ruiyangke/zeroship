# Workflow coordination and worker-owned persistence

**Status:** Implementation target revised after the ownership review. The
customer's worker processes customer data, and workflow persistence stays in
the customer's database and object storage. The workflow server remains a
lightweight coordinator for registry, placement, wake-up hints and high-level
management. The previously proposed data-owning server violated that boundary.

The [workflow reference](../reference/workflows.md) describes the existing
Control/Gateway dispatch implementation. The replacement engine, embedded task
runner, V8 lifecycle adapter and payload contracts are implemented in the
refactor branch, but their production composition remains unfinished. The engine
now requires an explicit customer schema binding and host-owned policy
snapshots. Its reserved journal tables contain execution data; admission policy
does not come from customer SQL or a platform database join.
The obsolete central completion and
payload protocol and its platform migration have been removed.
Closed coordination messages now live in `zeroship_core::workflow_coordination`,
alongside shared lifecycle metadata and validated identifiers/counters. Native
wire tests reject execution data and credentials at the message boundary,
including nested management operations and acknowledgements. The coordinator
metadata store, HTTP host and platform migration now use the metadata-only
contract. Native PostgreSQL and real-process tests cover scoped authority,
replica retries, startup privileges and restart recovery. Worker/CLI engine
composition remains unfinished.

This design supersedes the older
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
It preserves their customer-owned journal boundary while replacing Control's
workflow scheduling and the internal workflow-advance request path.

## Ownership

```text
Platform Control and Gateway
  app/deploy authorization, routing, usage accounting
                    |
Workflow coordinator
  worker registry, placement, wake-up hints, high-level management
                    |
                    | metadata, assignments and control commands
                    v
Customer's worker
  +-- app HTTP runtime / env.workflows
  |                         |
  +-- Rust workflow engine <-+
  |     acceptance, schedules, timers, signals, leases, history, retention
  |                         |
  +-- bounded task runner <-+
  |     pinned deployment -> V8 -> typed outcomes
  |                         |
  +-------------------------+----> Customer's database
  |                                 business tables
  |                                 reserved workflow journal tables
  |
  +------------------------------> Customer's object storage
                                    workflow payloads
                                    retained executable snapshots
```

The workflow engine is a Rust library embedded in the worker. The workflow
server is a separately deployable coordinator that never executes the engine's
customer database operations. The V8 adapter executes app code and returns
outcomes to the worker engine. Customer isolation and storage placement are
host configuration, never an argument chosen by app code.

| Component | Responsibility |
| --- | --- |
| Platform Control | Authorize app management and deployment, distribute trusted placement and policy metadata, and accept infrastructure usage records. No workflow journal connection or payload credentials. |
| Workflow coordinator | Register workers, assign customer scopes, retain wake-up hints and coordinate high-level management through metadata-only contracts. No customer journal connection or payload credentials. |
| Gateway | Route authenticated requests to the assigned worker. It does not schedule workflow steps or persist their data. |
| Customer worker, Rust host | Own workflow acceptance, scheduling, leases, history writes, signal delivery, payload access and retention through its resolved customer storage binding. |
| Customer worker, V8 | Execute the pinned app code with app-scoped native handles and replay input. No database connection, task token or raw storage credential enters V8. |
| Customer database | Authoritative workflow history, run state, durable ready work, timers, signals, leases and payload references. |
| Customer object storage | Large inputs/results and executable snapshots retained for replay. |
| Provisioning host | Apply the canonical migration definition with explicitly authorized customer migration credentials. Runtime workflow operations use ordinary DML. |

The platform may route customer requests as it does other app traffic; routing
does not authorize storing their contents in platform workflow tables or logs.
Management commands are authorized and routed by the platform and applied by
the customer's worker. History, inputs, outputs, signals and errors remain
customer data, including when a management UI requests them. The coordinator
may retain explicitly defined lifecycle and operation-acknowledgement metadata;
that permission does not extend to arbitrary result or error JSON.

## Coordinator contract

Separate placement ownership from execution ownership. Coordinator assignments
determine which worker may serve a customer scope. Customer database claims
determine which execution owns a run and which history committed. A registry
entry, wake hint or management acknowledgement cannot replace the database's
generation and lease checks.

| Coordinator record | Allowed contents |
| --- | --- |
| Worker registration | Worker identity, capabilities, placement attributes, readiness and heartbeat state. |
| Placement assignment | App/scope identity, worker identity, deploy identity, assignment revision and bounded authorization. |
| Wake-up hint | Scope identity, next due time and revision needed to discard stale hints. |
| Management command | Authorized app/run identity, a typed lifecycle operation, request identity and actor provenance. |
| Management receipt | Request identity, acknowledgement and explicitly defined lifecycle state. |

These are closed typed contracts. They contain no customer database URL, schema
credential, arbitrary execution envelope, checkpoint history, signal body,
workflow input, output or payload location. Customer storage bindings are resolved
inside the authorized worker host. Worker registration does not upload its
database credentials to the coordinator.

The coordinator may persist these records in platform storage and run replicas.
Existing service authentication and worker enrollment can protect the protocol.
Its database role must not gain customer schema privileges or a workflow journal
connection. `zeroship-workflow-server` is the coordinator host; its data-owning
HTTP task poll/complete/upload protocol is replaced rather than exposed as an
alternate mode.

`zeroship-workflow-server/src/coordinator.rs` owns the metadata store. Placements
are keyed by app and worker, with retained revisions and mutation receipts across
release and reassignment. Registration reports liveness and app-placement
capacity; renewing it cannot revive an expired assignment. Worker mutations
check app, instance, assignment revision and database time after acquiring locks.
The compio pool bounds both acquisition and the complete metadata transaction.

Lifecycle commands use explicit database columns, with closed acknowledgement
codes and lifecycle states. Retried requests retain their original command and
authenticated Control issuer. Delivery can repeat after a lost response or reach
another assigned worker; applying it still requires the customer's engine to
deduplicate by request identity in its own database.

The worker publishes wake-up metadata from a durable intent committed beside
its own scheduling changes. Publication is retryable and revisioned. A wake-up
only asks the worker to inspect its customer journal; duplicated or stale hints
cannot execute a step or mutate history. Lost-worker recovery reassigns the
customer scope and scans its durable state, including changes committed before
their metadata could be published.

A worker cannot relinquish responsibility on the assumption that an
unacknowledged hint was persisted. Scaling a scope to zero requires a durable
coordinator acknowledgement and a recovery path for lost assignments; until
that contract is implemented, keep a responsible worker host available.
The metadata store therefore refuses the last worker's voluntary release, even
after acknowledging its wake hint. Concurrent releases serialize by app, and
an expired registration or placement exposes the scope for recovery without
requiring a previously published hint. Host-driven reassignment and wake-up
delivery remain part of the production composition work.

Pause, resume, cancellation and restart commands carry identities and typed
options to the worker, where their effects commit in the customer database.
Operations carrying business data, such as start inputs and signal payloads,
use the customer's authenticated worker endpoint. The coordinator does not
persist them in its command queue. Detailed history/output reads also terminate
at the worker rather than a central copy of customer history.

## Rust composition and isolation

`zeroship-workflow` owns the reusable state machine, store contract, scheduling,
replay types and native task runner. `zeroship-workflow-v8` installs
`env.workflows` and implements the execution lifecycle. Worker and CLI hosts
construct the engine with a trusted app identity, resolved customer database
binding, customer payload store, immutable-deploy loader and runtime policy.

`WorkflowService` currently names the reusable engine object. An in-process
object with that name is not a separate platform service. The composition must
make its customer scope explicit; constructing the engine must not open the
platform database or select a global journal by default.

An app handle cannot nominate another app, schema, database, run owner or object
namespace. App identity comes from the trusted runtime binding, including for
Rust callers. `APP_ID` supplied through environment variables is not authority.
The physical schema is resolved independently of app identity, following the
[data-system contract](../architecture/data-system.md); it is not inferred by
concatenating an app ID into a database URL or schema name.

Database roles restrict a customer's worker to its authorized storage. Where
apps share a customer database, journal predicates, keys, foreign keys and
native handles also carry app identity. Sharing a database does not authorize
an app to inspect or modify another app's workflow. Reserved journal tables
remain inaccessible through creator collection and raw-query surfaces.

Worker-owned workflow writes are not privileged platform operations. They use
ordinary parameterized SQL in the customer's resolved schema, consistent with
the repository's process privilege invariant. No platform system schema,
definer-rights wrapper or worker-minted service capability creates a fictitious
boundary around them. A compromised worker process is outside the V8 app
isolation boundary and can reach credentials owned by that worker.

## Journal and task ownership

The authoritative tables live in the customer's database under reserved
`__zeroship_workflow_*` names. History, run generations, signals, timers,
task leases, completion receipts and payload references share that database's
transaction domain. The existing original journal uses
`__zeroship_workflow_runs` and `__zeroship_workflow_steps`; the replacement
schema must preserve customer placement while adding the shared engine's
required records.

The canonical migration DSL owns PostgreSQL and SQLite schema generation.
Provisioning receives a validated customer schema binding. Runtime startup
verifies readiness, and claiming a task never creates tables. An incompatible
local database requires an explicit workflow reset; it is never silently
replaced or reopened through the old engine.

```text
start(input)
    |
    v
Worker Rust engine -- customer DB transaction --> run + durable ready work
    |
    +--> return accepted run handle

Worker task runner -- customer DB transaction --> claim + lease
    |
    v
V8 replay and bounded app execution
    |
    v
Worker Rust host -- customer storage --> confirm large payload uploads
    |
    v
Customer DB transaction
    validate lease
    commit history + payload references + run state + next ready work
```

Workers authorized for the same customer journal coordinate through its
transactions and leases. A task is bound to app, run, generation and lease
epoch. Returned JavaScript values cannot choose the mutation target. Stale
owners cannot renew, complete or attach payload references after losing
authority. Completion retries retain the same task and outcome identity.

Timeout, cancellation and loss of confirmed lease stop or quarantine the
isolate and join native work before releasing execution capacity. A monotonic
host deadline covers synchronous JavaScript as well as asynchronous operations.
Renewal cannot revive an already expired execution.

The worker engine, not Control, discovers ready tasks and advances due timers,
scheduled occurrences, child waits and compensation retries. Notifications are
hints; durable state is rediscovered after restart. V8 eviction does not remove
an app's scheduling registration or durable work.

A customer needs an available worker host for background progress. The
coordinator's registry and wake-up hints provide placement and wake-up inputs;
the worker's customer journal remains the authority for due work. Scaling down
must follow the acknowledgement and recovery contract above. The coordinator
never discovers due work by reading customer journals.

Platform policy reaches the worker as trusted runtime metadata through the
existing assignment/configuration path. Admission uses that metadata with an
explicit validity and outage policy; metadata cannot require a platform SQL
connection in each workflow transaction. Admission expiry stops new work while
retaining history and allowing bounded shutdown and necessary recovery. Do not
claim atomic transactions across platform policy and the customer database.

## Payloads and replay

Small values remain inline in customer history. Large or explicitly referenced
values use the customer's configured object storage. V8 returns results to the
Rust host; it does not write the journal or hold raw payload credentials.

The host stops the app isolate before uploading results. Prepared uploads keep
their bytes, content descriptors and request identities through transport
retries, without executing the callback again. Upload state and reference
ownership are tracked in the customer journal. Completion promotes confirmed
uploads and commits history in the same transaction. Failed or abandoned uploads
remain collectible, and collection serializes with reference promotion.

Replay resolves references through the assignment's captured generation and
app scope. It verifies content and enforces buffering limits. A missing or
corrupt required payload is an infrastructure failure: interrupt execution and
leave the attempt retryable, rather than letting app code catch the failure and
commit a different history. Customer payloads are never uploaded to a platform
workflow endpoint as part of executing or completing a step.

Stable operation identities cover run generation, retained origin and execution
phase. Retries preserve identities for repeated effects; explicit restart creates
new identities for re-executed work. External effects can repeat when an effect
succeeds before its checkpoint commits. The SDK must expose this boundary and
provide an idempotency key for destinations that support one.

## Triggers, lifecycle and deployments

App `start()` and signal operations call the worker's bound Rust engine directly.
HTTP signals and creator management calls reach authenticated worker operations
through normal routing. Capabilities bind the app, permitted operation and
target, and are checked at the worker; paths alone are not authority. Neither
Control nor Gateway opens the customer's journal to complete such a request.

Schedules, sleeps, signal delivery, topic fanout, child workflows, continuation,
restart and compensation use the same customer-owned state machine. Changes
that race a task completion are resolved transactionally. Pausing or cancelling
must not erase a signal or a pending child transition. Restart requires
quiescence and retains only the explicitly selected immutable replay prefix.

Deployment snapshots are durable in customer storage before accepting runs
pinned to them. Their content identity covers the executable module and its
dependencies. A cache entry or a live Vite URL is not a durable deployment pin.
New deployments affect new runs and schedule reconciliation; existing runs
continue with their retained snapshot. Missing snapshots park affected work.

Journal-aware retention runs at the customer worker and preserves snapshots,
payloads, restart generations and child/parent dependencies as required. Platform
bundle collection must not require SQL access to customer journals; runnable
work uses the durable customer snapshot rather than an evictable platform cache.

The JS replay interpreter has a shared implementation consumed by the runtime,
SDK and bootstrap. The local host does not maintain a smaller workflow engine
with different lifecycle or replay semantics.

## Local development

`zeroship serve` embeds the same Rust engine and task runner with SQLite, local
payload files and retained executable snapshots. The CLI persists a trusted
project app identity under `.zeroship`; the creator need not set `APP_ID`.
Separate projects receive separate storage and identities by default.

Stopping the CLI preserves journals and snapshots. Startup discovers due work
and expired leases. Hot reload installs a new immutable active deployment while
old runs retain their original code. An explicit workflow reset removes only
workflow-owned state, payloads and unneeded snapshots, leaving business DB, KV
and storage data untouched.

Programmatic Rust construction and TOML resolve to the same validated host
configuration. Customer connections and storage credentials use the existing
secret handling. Local development needs no coordinator daemon, Control instance,
network service identity or PostgreSQL server.

## Implementation and verification

Keep the reusable native state machine, lease-aware runner, V8 shutdown barrier,
payload preparation and failure contracts. Replace their storage and host
composition with the customer-bound form. Retain a lightweight coordinator;
centralized history and payload ownership are not a temporary production mode.

- Bind journal schema, app identity, database role and payload storage through
  the worker's resolved customer context. Generate the reserved customer tables
  through the canonical migration DSL for PostgreSQL and SQLite.
- Replace the server's data-owning task transport with registry, placement,
  wake-up and management contracts. Remove the platform workflow journal and
  its roles, remote payload uploads and workflow-specific Control journal SQL.
  Keep platform persistence limited to explicitly defined coordination metadata.
- Compose acceptance, maintenance and task runners in the customer worker with
  bounded slots independent of request isolates. Keep scheduling active across
  V8 eviction and load retained snapshots for replay.
- Compose that engine in the CLI and remove the old local mini-engine. Complete
  shared interpreter, retry, effect identity and payload behavior before cutover.
- Route management and public signal operations to the customer's worker under
  scoped authority. Remove Control/Gateway workflow-advance dispatch and
  incompatible configuration in the same producer/consumer cutover.
- Complete native Rust and self-contained example coverage before reporting the
  replacement as deployed behavior. Do not preserve obsolete APIs as aliases.

| Contract | Required evidence |
| --- | --- |
| Customer data boundary | Customer journal and blob contents exist only in the configured customer stores. A platform runtime has neither journal credentials nor table privileges. |
| App isolation | Foreign app reads, writes, child edges, signals, references and forged runtime scope are rejected, including when apps share a customer database. |
| Worker independence | A customer worker discovers, executes, checkpoints and recovers durable work using its own stores without a platform journal connection or central task-completion service. |
| Durable acceptance | Concurrent starts, schedule occurrences and lost responses preserve request identity and accepted runnable work. |
| Lease fencing | Worker death, expired authority, stale completion, cancellation and synchronous app execution preserve safety and permit recovery. |
| History and payloads | Upload/commit failures, corrupt reads, app background tasks and collection races cannot admit invalid references or delete retained results. |
| Lifecycle | Signal, child, pause, cancellation, restart and compensation races have matching PostgreSQL and SQLite behavior. |
| Deployment | Restart and hot reload retain executable snapshots; missing code is visible and never replaced silently with the current deployment. |
| Local parity | Worker and CLI exercise the shared engine, including scheduling, children, continuation, compensation and large outputs. |

Required environments belong to Testcontainers using major image tags, and
database outages fail required tests rather than skip them. Native Rust tests
own engine and host verification through `cargo xtask test workflow`. Each
example owns its Vitest and Playwright tests and fixtures. No new bash workflow
test orchestration is introduced.
