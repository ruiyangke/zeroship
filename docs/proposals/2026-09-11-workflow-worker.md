# Workflow coordination and worker-owned persistence

**Status:** Implementation target revised after the ownership review. The
customer's worker processes customer data, and workflow persistence stays in
the customer's database and object storage. The workflow server remains a
lightweight coordinator for registry, placement, wake-up hints and high-level
management. The previously proposed data-owning server violated that boundary.

The [workflow reference](../reference/workflows.md) describes the existing
Control/Gateway dispatch implementation. The replacement engine, worker task
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
replica retries, startup privileges and restart recovery. The CLI now composes
the shared engine and background runner. Production composition remains
unfinished. The CLI and Vite supply the normal app archive. The engine and V8
runner load its manifest and content-addressed blobs; workflow executable
snapshot persistence and its separate size limit have been removed.

The normal bundle crate now owns executable loading and manifest-identity
verification. Ingest preserves the manifest fields used to compute deployment
identity; the CLI verifies that identity before loading code. Native tests load
an earlier deployment after installing a replacement and reject corrupt,
missing or oversized sources. The workflow-specific content hash has been
removed. The platform deployment hold ledger now uses ORM models generated
from the normal deployment migration. Acquisition, release and reclamation
serialize on the deployment record; generations fence stale retries and released
rows remain as tombstones. Native PostgreSQL and SQLite tests cover retries,
reconnection, scope and reclamation, including a concurrent PostgreSQL collector.
The customer journal now records acquisition and release intents through ORM
models. Scoped clients reconcile lost replies and cancelled calls; acknowledgements
must match the full intent identity and its current generation. Release closes
deployment admission under the app lock and refuses retained runs, generations
or schedules. Executable preparation fences retirement across storage I/O.
Native PostgreSQL and SQLite tests exercise these transitions against the real
metadata ledger, including stale acknowledgements and foreign app references.
The local host now registers normal app deployments in an ORM-backed catalog
beside their manifests and blobs. Deployment identities survive journal
replacement, and activation acquires a durable hold using the local journal's
stable host identity. Repeated publication preserves reclamation tombstones.
Engine activation now acquires or reconciles a hold through a host-bound client
before verifying the normal app artifact. Both activation and task reads check
the held generation across artifact I/O. Missing or corrupt artifacts park
their deployment, and an availability epoch protects concurrent repairs.
The shared worker now reconciles pending holds in background maintenance through
host-bound clients. Recovery rotates between assigned apps, advances past failed
or malformed intents and bounds each attempt with the existing maintenance
timeout. Expired policy leases do not discard retention intent; recovery does
not activate deployments or admit execution. Each worker loop has its own wake
queue entry so continuous task polling cannot starve journal maintenance.
Native PostgreSQL and SQLite contracts exercise restart, lost replies, corrupt
records, foreign assignments, timeout and shutdown. The production retention
cutover remains pending.

The existing production worker still uses `claim_workflow_run`, which joins
customer journal rows with `zeroship.apps`, `zeroship.plans` and
`zeroship.app_deploys`. Its startup database posture requires those platform read
grants. These paths and grants violate the revised ownership rule and must be
removed with production composition; the new engine's customer binding does not
establish that boundary for the old worker binary.

The native `zeroship_workflow::coordination::WorkerCoordinator` client exchanges
registration, assignment, wake and management metadata through the authenticated
service API. It binds the enrolled worker signer, mints a fresh assertion per
call and checks response
scope and receipt identity. Request serialization, response streaming and the
complete exchange are bounded. Redirects and malformed metadata are rejected.
The production worker's polling and policy composition remain pending.

Active, reloaded and pinned production worker isolates now load the complete
module graph through the shared bundle loader. Pinned manifest reads verify
their canonical content hash. Runtime bootstrap preserves creator module paths;
static and dynamic imports resolve relative to their importer and share module
records. Native tests exercise an original pinned deployment beside its active
replacement, lazy dependencies, import failures and top-level await.

The replacement service now uses a shared `OrmStore`; its workflow-owned
PostgreSQL and SQLite adapters have been removed. The CLI supplies its normal
database binding and object storage. Transaction ownership stays on the engine's
compio thread, with a bounded app-scoped client for V8 and other Rust threads.
Journal fingerprint and deployment reads, root-run creation, request receipts,
checkpoint persistence, wait cleanup and lifecycle transitions use ORM models
and collections. History reads seek through ordered pages, and status joins each
run to its current generation using the complete app-scoped key. Invocation and
restart load generation inputs through the same generated model. Restart checks
live leases and waiting children through the ORM, then atomically expires tasks,
cleans up waits and signals, and advances the generation. Retained checkpoints
and payload references are copied through ordered ORM pages and batch inserts
inside that transaction. The copy preserves effect origins and compensation
metadata, and retains only the selected prefix and input references. Graph
checks traverse app-scoped ORM pages with cycle detection and a bounded work
budget. Every journal and coordinator table has a sole `id` primary key;
composite domain identities use unique indexes. Restart copies allocate fresh
row IDs while retaining the recorded effect origin.
Journal reads and writes use ORM collections. The engine uses main's callback
transaction API; dropping its local journal handle requests rollback, and
success waits for confirmed commit. Database-clock SQL runs through a separate
connection to the same customer database so it cannot contend for the journal's
held pool lease. Clock reads observe no journal rows.
Integration with main exposes a remaining ORM blocker: descriptor installation
and collection handles still reject the reserved workflow table prefix.
The journal compiles against the public API, but cannot open on either backend
until that native access is supported. This failure is recorded for the ORM
owner; workflow retains its table names and leaves main's ORM unchanged.
The existing Control store's removal remains pending.

Task authorization re-reads the generated task model after acquiring the app
and run locks, retaining the original scope and generation identity. Lease
recovery, claims, heartbeats, completion receipts and release use conditional
ORM updates and check the affected row count. Native contracts cover stale
completion, expired leases and a corrupt run reference to another app's task.
Heartbeat and completion recheck database time after their final mutation;
delayed-write tests prove an expired lease rolls back its receipt, history and
deadline changes before another worker recovers the run.
Payload admission and upload confirmation also recheck the lease after their
database writes. A delayed admission leaves no upload record; a delayed
confirmation leaves the original upload collectible instead of acknowledging
staged content under expired authority.
Task, schedule and broadcast discovery filter host-assigned apps before their
batch limits, so a foreign backlog cannot starve the assigned scope.
Signal delivery, subscription sequencing, broadcast publication and recipient
joins now use generated ORM models and collections. Mailbox reads retain their
inclusive time bounds and generation targets; consumption checks the affected
row count before committing the checkpoint. Broadcast recovery keeps its durable
recipient cursor and publication cutoff across batches and restarts.
Completion, compensation and continuation writes also use ORM collections.
Compensation errors are read in descending ordinal pages, and continuation
retargets parent checkpoints using the complete run, generation and ordinal
cursor. Native tests cover histories and parent waits that span query pages
without crossing app or generation boundaries. Waiting parents are discovered
through app-scoped ORM pages and woken by generation-checked updates under the
app lock.
Schedule reconciliation scans historical entries in ordered ORM pages, preserving
schedule identity and unchanged due times across activation retries. Occurrences,
overlap checks and run creation use the shared ORM transaction. Revision overflow
refuses activation and rolls back schedule changes on either backend. Due
schedule discovery uses typed joins with host scope applied before limits.
Signal-token authorization reads app and topic epochs through generated models;
revocation writes the scoped app, run or topic collection under the existing
locks. Native tests preserve foreign-app and unaffected-target authority,
idempotent revocation and epoch exhaustion behavior. Topic initialization reads
the existing epoch and inserts an absent topic through ORM collections while
holding the app lock. Concurrent issuers cannot reset a revoked epoch, including
when they use independent worker connections.
Payload admission, lookup, promotion and collection state changes now use ORM
collections and generated models. Replay and slot reads join payload ownership
with the complete app and generation scope. Quota aggregation uses native integer
or exact decimal results without floating-point conversion. Collection discovery
uses host-scoped ORM reads; object deletion remains outside the journal
transaction and retains the durable deleting state and tombstones.

This design supersedes the older
[control-plane design](2026-07-05-durable-workflows-design.md),
[scheduler registration design](2026-07-08-durable-workflows-scheduler-worker-design.md)
and [implementation plan](2026-07-05-durable-workflows-implementation-plan.md).
It preserves their customer-owned journal boundary while replacing Control's
workflow scheduling and the internal workflow-advance request path.

## Ownership

Workers may access only their authorized creator databases. Control and other
platform services may access only the control database. Sharing the Rust ORM
library does not share database authority: worker hosts receive no control
database credentials, and platform services receive no creator database
credentials. Cross-boundary management and deployment-hold operations use
authenticated service contracts; each receiving process writes only its own
database. The existing Control journal-reading operations violate this boundary
and must be removed during the replacement.

The final deployment places creator workers and platform services in separate
zones. Each database is private to its zone. Neither side may require a network
route to the other database, a cross-database query or shared connection
credentials. The worker initiates coordination requests to the authenticated
platform endpoint and polls for assignments and management commands. Control
queues those commands without opening a connection into the creator's database.
HTTPS protects the remote connection; literal loopback HTTP supports local
hosts. Sharing infrastructure in a local development environment must not become
a dependency of the production protocol.

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

Customer's worker ----------------> App deployment bundle store
  load the run's pinned deployment   existing hosted app manifests and blobs
                                     retained by platform deployment metadata
```

The workflow engine is a Rust library embedded in the worker. The workflow
server is a separately deployable coordinator that never executes the engine's
customer database operations. The V8 adapter executes app code and returns
outcomes to the worker engine. Customer isolation and storage placement are
host configuration, never an argument chosen by app code.

Persistence is being migrated onto the existing Rust ORM. Platform services and
the customer worker are native ORM consumers; V8 is an adapter to that same
library. Workflow code must not maintain PostgreSQL and SQLite implementations
of connection management, parameter binding, row decoding or transaction cleanup.
The replacement service uses this backend composition. Collection conversion
and production cutover described below remain implementation targets.

| Component | Responsibility |
| --- | --- |
| Platform Control | Authorize app management and deployment, distribute trusted placement and policy metadata, and accept infrastructure usage records. No workflow journal connection or payload credentials. |
| Workflow coordinator | Register workers, assign customer scopes, retain wake-up hints and coordinate high-level management through metadata-only contracts. No customer journal connection or payload credentials. |
| Gateway | Route authenticated requests to the assigned worker. It does not schedule workflow steps or persist their data. |
| Customer worker, Rust host | Own workflow acceptance, scheduling, leases, history writes, signal delivery, payload access and retention through its resolved customer storage binding. |
| Customer worker, V8 | Execute the pinned app code with app-scoped native handles and replay input. No database connection, task token or raw storage credential enters V8. |
| Customer database | Authoritative workflow history, run state, durable ready work, timers, signals, leases and payload references. |
| Customer object storage | Large workflow inputs and results. |
| App deployment bundle store | The app's existing deployed code, dependencies and runtime descriptor. Workflow execution reuses this artifact under a durable deployment hold. |
| Creator-side provisioning host | Apply the canonical migration definition using the creator's authorized database connection. Platform services do not receive that connection. Runtime workflow operations use ordinary DML. |

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

Production management execution must retain its outcome for the coordinator's
entire redelivery lifetime. The engine's ordinary request receipts expire, while
a queued coordinator command currently remains pending until acknowledgement.
Connecting polling directly to `transition` or `restart` would therefore allow a
lost acknowledgement and delayed retry to repeat a restart after receipt expiry.
The worker needs durable management receipts, including rejected outcomes,
before that connection is enabled. Acknowledgements remain closed metadata;
their durability must not require either service to read the other's database.

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

Database roles restrict a customer's worker to its authorized storage. Workflow
handles, journal predicates, keys and foreign keys carry app identity. Direct
ORM access is transparent within the authorized customer schema, including
workflow tables. Table prefixes communicate ownership and are not an access
boundary. Apps granted the same schema share its data; separate schema bindings
and database permissions enforce separation where required.

Worker-owned workflow writes are not privileged platform operations. They use
ordinary parameterized SQL in the customer's resolved schema, consistent with
the repository's process privilege invariant. No platform system schema,
definer-rights wrapper or worker-minted service capability creates a fictitious
boundary around them. A compromised worker process is outside the V8 app
isolation boundary and can reach credentials owned by that worker.

## ORM migration

The worker host supplies the app's resolved ORM database binding and customer
object storage. A workflow repository expresses journal operations against that
binding. The ORM selects PostgreSQL or SQLite from normal host configuration and
owns connections, native values, statement execution and transaction settlement.
The CLI uses this composition alongside its ordinary app services; it does not
resolve a separate workflow database or introduce another database lifecycle.

```text
Customer worker
  workflow repository ----+
  app Rust operations ----+--> zeroship-data-orm --> configured customer database
  app V8 database binding-+
```

Use existing ORM model and collection operations for supported reads, inserts,
updates, deletes and transactional changes. Query compilation belongs to the
ORM. Parameterized SQL remains only where the existing public API cannot express
the required operation or internal table contract. In particular, preserve
database-time lease checks, affected-row compare-and-set decisions, app locking,
generation fencing and atomic history/reference updates during the conversion.
A read followed by an unconditional write is not a substitute for an atomic claim.

The ORM permits all table prefixes within the host-bound schema for both Rust
and creator code. It validates identifier syntax and uses normal schema binding
and database permissions; no special internal collection role or prefix allowlist
is needed. Descriptor-defined identity and mutation rules still apply. Use the
existing scoped ORM execution interface for operations not yet represented by
model operations, keeping remaining SQL explicit and reviewable.

Rust models follow the ORM's existing migration-derived model contract. Emit a
runtime descriptor for the physical reserved table names from the same canonical
migration definition, then use `orm::schema!` to generate entity and column
metadata. `FromRow` defines typed read projections, `Insertable` defines creation
inputs, and `Changeset` defines explicit partial updates. Do not maintain another
handwritten schema in Rust or JSON. The workflow generator emits DDL,
fingerprints and the runtime descriptor from the canonical migration. Native
PostgreSQL and SQLite contracts compare descriptor columns and keys with the
migrated catalog. The engine installs these models alongside the app's existing
descriptors, refusing conflicting entries before publication. Fingerprint and
deployment reads, root-run creation, request receipts, status and checkpoint
operations use that metadata. Native tests cross the history page boundary and
reuse run IDs across apps and generations to verify complete, scoped reads and
writes. The remaining journal conversion is unfinished.
The public Rust and TypeScript ORM accepts declared named and composite keys.
Bounded mutations, joined row projections, immutable-field checks, concurrency
predicates and encrypted-row identity use every key component. Live database
tests cover composite encrypted writes, unmasking, rollback and conflicting
upserts across generations. Workflow models and remaining SQL share an owned
`orm::Transaction`; model operations use its scoped `Database`. Cancellation,
rollback and commit settle through the shared protocol. Preserve conditional
updates when converting the remaining journal operations.

Extend the shared ORM where workflow models need a general table capability.
Do not introduce workflow-specific database adapters or another query builder.

The conversion must preserve ordinary customer-role permissions, native app
isolation, rollback after cancellation, and refusal of indeterminate commits.
Opening a workflow repository does not provision PostgreSQL roles or tables.
The canonical migration DSL continues to own the journal schema. Local setup
may initialize the reserved journal alongside business data through the shared
host setup, without deleting or replacing that data.

Native contracts must execute the same workflow operations through the ORM on
PostgreSQL and SQLite. Testcontainers owns required database services. Exercise
shared app storage, concurrent claims, stale completions, transaction failure
and restart recovery before removing the workflow-owned adapters. Example
acceptance tests remain self-contained Vitest and Playwright projects.

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
local database requires an explicit operator action; startup never silently
replaces it or reopens it through the old engine.

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

Workflow classes ship inside the app's normal `.zship` deployment. Each run
generation pins the trusted app identity and an immutable app deployment ID.
The existing deployment manifest supplies the canonical content hash, and the
normal bundle loader verifies it. Do not duplicate that hash as a separate
workflow-owned version contract. The deployment ID must permanently resolve to
the same artifact; deployments cannot be overwritten in place. Its worker
loads that deployment through the normal app manifest and blob loader, including
the pinned runtime descriptor and dependencies. There is no separate workflow
bundle, upload, executable snapshot store or `--workflow-bundle` option in the
target interface. New deployments affect new runs and schedule reconciliation;
existing runs continue using their original app deployment. Missing or corrupt
code parks affected work; it never selects the current deployment as a fallback.

The platform retains its existing app artifact through durable deployment holds.
The customer's worker determines journal dependencies, including retained restart
generations and child/parent edges. It reports retention metadata under scoped
host authority. Neither the coordinator nor deployment garbage collection reads
customer SQL or receives history, payloads or source-code copies.

The hold protocol must close the admission-versus-deletion race:

- The worker durably records acquisition intent before requesting an app-scoped
  deployment hold. Platform acquisition and bundle reclamation serialize on the
  same deployment record. A deployment being reclaimed cannot gain a new hold.
- The worker admits runs only after the platform acknowledges the hold and the
  worker records it durably. A lost response is retried using the same operation
  identity; accepting a run and sending an asynchronous pin afterward is unsafe.
- Before release, the worker closes admission for that deployment and verifies
  that its customer journal has no remaining replay dependencies under the app
  lock. It records a durable release intent. New admission must reacquire a hold
  before reopening the deployment.
- Hold generations and scoped host authority fence stale releases. Retries of an
  old release cannot remove a reacquired hold. Ownership handoff preserves pending
  intents and existing holds.
- Holds survive worker disconnection, placement expiry and coordinator restart.
  A missed heartbeat or an unreachable customer database is never evidence that
  a bundle is reclaimable. Failed release delivery retains the bundle until the
  idempotent operation is acknowledged.

Journal-aware payload retention stays in the customer worker. Platform bundle
collection consults platform-owned deployment holds, active routing and other
deployment consumers. The existing Control journal-reading retention sweep must
be replaced as part of the production cutover.

The JS replay interpreter has a shared implementation consumed by the runtime,
SDK and bootstrap. The local host does not maintain a smaller workflow engine
with different lifecycle or replay semantics.

### Upgrading a running workflow

New runs use the active app deployment. Existing generations keep their pin.
A full restart can explicitly select the latest deployment and execute from the
beginning with the original input. A partial restart keeps its existing deployment
because it retains history produced by that code. Full restart may repeat external
effects and is distinct from preserving progress across an upgrade.

Mid-run upgrades are a proposed checkpoint handoff, not an implemented API.
The app declares a safe checkpoint, a versioned business-state contract and a
resume path in the target deployment. State transformation must be bounded and
free of external effects. The engine does not infer how to migrate a JavaScript
stack or replay old execution history through changed code.

The customer worker quiesces the source generation, retains the target app
deployment and validates the target's resume state. It commits the handoff in
the customer database: recheck the source generation and authority, record the
upgrade, persist the resume state, fence old execution and enqueue a new generation
under the same run ID with the target deployment pin. Transformation failure or
a stale source leaves the handoff uncommitted. Recovery follows the committed
generation; retrying an acknowledged upgrade cannot create another generation.

History remains associated with the deployment that produced it. Signals arriving
during a handoff remain durable. Timers, child relationships and compensation
require explicit carry-over rules; unsupported outstanding operations refuse the
upgrade. External effect identities belong to the transferred business state
where required to prevent repeated effects. Target deployment holds must precede
the handoff commit, and source holds remain while replay or restart depends on them.

## Local development

`zeroship serve` embeds the same Rust engine and task runner. Its journal belongs
in the app's resolved database, alongside business tables under the reserved
workflow table prefix. SQLite development uses the app's existing SQLite file;
the host supplies the database binding instead of a workflow-specific database
path or environment variable. The CLI now supplies this shared-database binding
and the app's normal object store. The workflow-specific CLI reset, filesystem
deletion protocol and reset locks have been removed.

The CLI remains a single-app host. It resolves the normal app code, database and
storage configuration, then initializes the shared workflow engine and runtime
binding. Worker lifecycle, durable execution and retention belong to the shared
worker implementation. Do not add workflow-specific deployment management or
file polling to the CLI. Supporting multiple app deployments in the CLI is a
separate design question and is outside this work.

Local development builds the normal app deployment and retains
its manifests and content-addressed blobs locally. The normal deployment catalog
at `.zeroship/deployments/index.sqlite` records deployment identities and holds;
it contains deployment metadata, while execution history stays in the app's
existing database. No workflow database path or extra environment variable is
needed. HTTP and workflow execution
derive from the same app build; creators do not supply a workflow-only entry or
archive. The CLI persists a trusted
project app identity under `.zeroship`; the creator need not set `APP_ID`.
Separate projects receive separate storage and identities by default.

Stopping the CLI preserves journals and retained app deployments. Startup discovers due work
and expired leases. Hot reload installs a new immutable active deployment while
old runs retain their original code. Any cleanup through the shared engine must
preserve business tables, app deployment artifacts, KV and storage data.
App deployment collection applies its own
reference checks before reclaiming bundles no longer needed by any local consumer.

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
- Replace workflow-owned database adapters with the shared Rust ORM. Convert
  supported journal operations to ORM operations and retain explicit SQL only
  for concrete public-API gaps. Shared ORM changes apply equally to Rust and V8.
- Replace the server's data-owning task transport with registry, placement,
  wake-up and management contracts. Remove the platform workflow journal and
  its roles, remote payload uploads and workflow-specific Control journal SQL.
  Keep platform persistence limited to explicitly defined coordination metadata.
- Compose acceptance, maintenance and task runners in the customer worker with
  bounded slots independent of request isolates. Keep scheduling active across
  V8 eviction and load the pinned app deployment for replay.
- Replace workflow-only archives and executable snapshot copies with the normal
  app bundle loader. Establish durable deployment holds before admission and
  replace platform journal reads with the metadata retention protocol.
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
| Deployment | HTTP and workflow code come from the same app artifact. Restart and hot reload preserve pinned deployments. Missing code is visible and never replaced silently with the current deployment. |
| Bundle retention | Admission, reclamation, handoff and lost acknowledgements cannot race away a needed deployment. Stale releases and expired placement cannot remove a live hold. Platform collection has no customer journal access. |
| Local parity | Worker and CLI exercise the shared engine, including scheduling, children, continuation, compensation and large outputs. |

Required environments belong to Testcontainers using major image tags, and
database outages fail required tests rather than skip them. Native Rust tests
own engine and host verification through `cargo xtask test workflow`. Each
example owns its Vitest and Playwright tests and fixtures. No new bash workflow
test orchestration is introduced.
