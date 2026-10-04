# zeroship-workflow

The Rust workflow engine. It owns the journal protocol, replay and folding,
the app-scoped backends a host composes, and the workflow service that holds
the journal. It does not depend on V8.

- `engine.rs`: dispatch envelopes, outcomes, and journal folding.
- `execution.rs`: typed replay inputs, executor outcomes and runtime decoding.
- `backend.rs`: the `WorkflowBackend`, `StepOutputReader` and `InputStager`
  traits a host implements, and the app-scoped handles built on them.
- `service/`: the workflow service - app handles, transactional lifecycle,
  journal storage and the worker task protocol, through the shared Rust ORM.
- `lifecycle.rs`, `operations.rs`, `validation.rs`: run lifecycle states, the
  operations a caller may request, and the checks applied to them.
- `deploy_registrations.rs`: the schedule registrations a deployment declares.
- `deployment_holds/`: scoped clients for deployment retention.
  `zeroship-workflow-manager::deployments` owns the platform ORM ledger and its
  canonical schema; `zeroship-workflow-client` owns authenticated transport.
- `zeroship-workflow-calendar`: shared parsing and timing semantics used by the
  manager; creator acceptance does not evaluate calendars.

The journal lives in the platform database, in the workflow service's own
schema, and `zeroship-workflow-schema` carries its generated artifacts. A
worker runs creator code and reaches the service for every journal fact. The
separate `zeroship-workflow-v8` crate installs `env.workflows` and supplies a
V8 executor.

The shared runner supplies `WorkflowInvocation` and receives `WorkflowExecution`.
Local and deployed hosts share journal types and outcome decoding. The host
keeps lease credentials outside the invocation and binds returned outcomes to
its claim before applying them.
`AppWorkflows::tasks` retains the exact app and policy generation for task,
payload and retained-artifact operations. A shared worker identity or valid task
token cannot broaden that handle to another app. `HostPolicies::run_bound` lets
native hosts prepare creator resources under the same registry and original
policy deadline, with cancellation on revocation and no authority replacement
through a later refresh.

The shared engine is composed into the CLI and into the worker, which binds it
through `WorkflowBinding::remote`. Scheduling and queue delivery belong to the
manager, and a worker executes only the bounded jobs the manager delivers. A
worker opens its creator database for `env.db`; Control and the other platform
services open the platform database. Authenticated service contracts carry
cross-boundary requests without sharing database credentials.
`AppWorkflows::management_job` accepts manager-delivered lifecycle commands
with explicit per-run revisions and durable journal receipts. Lifecycle
state, publication intents, command history, the applied revision and the job
receipt commit together. Started restarts use retained journal code; Latest
restarts verify and retain the exact deployment named by the command. Neither
path reselects a deployment after accepting the delivery. Repeated commands return their recorded
outcome after later lifecycle changes or policy expiry. Changed command
bodies conflict; infrastructure failures remain retryable. Lifecycle rejection
and database failure are separate paths, so a failed write cannot become a
permanent denial. Receipt compaction awaits a coordinator redelivery contract.
Management captures host authority before opening its journal transaction. Its
original deadline covers app-lock waits and settlement, even if the host refreshes
policy meanwhile. Missing authority permits only exact immutable receipt readback;
an unseen command remains retryable without writing a denial receipt.
App-operation `RequestId` receipts also persist without an age-based expiry.
Retries return their original result, and changed operation bodies conflict.
Lifecycle changes do not free an accepted request identity for reuse. Replaying
a signal-token request returns the original token, whose expiration and
revocation still apply. Receipt retirement requires explicit admission fences;
the journal does not infer them from elapsed time.
`service::publication` records immutable Advance, Fanout and Propagate intents
with creator transitions. Exact optional projections distinguish executable
frontiers from code-free broadcast and propagation pages. `AppWorkflows::pending_jobs` and `publish_job` publish under a bound
app scope without holding a creator transaction across manager I/O. The entire
returned specification must match before confirmation; retries preserve job
identities, and pending intents retain deployment dependencies independently of
run history. Release checks validate every pending app specification before
trusting its deployment projection. The workflow service, which holds the journal
and the queue, is the only publisher: its maintenance lane publishes through
`LanePublisher` and each app's publication wake drains intents after a commit
(`crates/zeroship-workflow-server/src/sweeps.rs` and `src/publication.rs`).
Workers publish nothing.
`service::delivery` accepts an advance job for its exact app, deployment, run,
generation and frontier. Native manager grants and authenticated client leases
implement the trusted Rust `JobLease` contract. The returned `DeliveredTask`
keeps its delivery identity and monotonic creator deadline private. Duplicate
live claims defer; completed jobs replay a retained semantic receipt without
running app code. Completion commits history, the outcome and successor intents
together. Renewal and mutation remain bounded by manager authority, creator
policy and the original task lease, including database waits. An expired attempt
can read its committed result but cannot admit new work. Receipt records survive
history removal; their presence also excludes the run from the old poller until
that path is removed. `JobReceipt::settlement` binds the outcome to the current
delivery; successor publication currently uses the durable outbox independently.
`AppWorkflows::activate_job` resolves the deployment hash through its scoped
hold receipt, verifies the normal bundle, and commits readiness, selection and
the logical job receipt together. Activation history remains independently
ready for queued jobs when a newer revision becomes selected. Exact retries
read the committed receipt without loading or reacquiring the artifact.
Captured delivery and policy authority fence app-lock waits, external I/O and
commits. Activation runs no creator code and schedules no occurrences.
`AppWorkflows::cron_job` binds a manager occurrence to its activated deployment,
reacquires the journal's deployment hold, and resolves static input from the
verified normal bundle. Under the app lock it commits the schedule identity,
occurrence, exact run, Advance publication intent and job receipt together.
Overlap skips retain a rejected receipt; capacity and unavailable prerequisites
remain retryable. Historical activations keep their original input even after
replacement or schedule removal. Receipt replay requires the exact occurrence
linkage and performs no artifact I/O.
`AppWorkflows::maintenance_job` (`service::maintenance`) routes activation, cron,
management, reconciliation, collection, fanout, propagation, hold release and
closure jobs to bounded journal operations; the workflow service's maintenance
lane claims those kinds. `runner::delivery::DeliverySlot` runs a claimed advance
job, the one kind a worker claims, through the executor and payload pipeline. Its
host supplies `JobTransport`; the authenticated worker client implements that
metadata interface. The slot renews manager and creator authority together,
retains interrupted execution until native shutdown joins, and retries exact
settlement after a committed creator result. This slot performs no journal
discovery or calendar evaluation.
`AppWorkflows::fanout_job` expands a delivered topic page inside one creator
transaction. Topic acceptance and completion sequences keep later broadcasts
pending until their predecessor finishes. The page's original subscription
cutoff, recipient signals, cursor, receipt and successor publication intents
commit together. Fresh progress follows the previous retained page and completed
topic head; exact historical pages replay after those heads advance. Waiting
means the next page is committed in the creator outbox, whose normal
reconciliation publishes it. Signal delivery sequence determines consumption
order; timestamps govern age and deadline eligibility. Direct signals and
different topics merge by materialization order. Fanout performs no deployment
lookup, storage access or creator execution.
`service::propagation` carries cancellation cascades and parent notification.
A settling generation that names cascading children, or a terminal head with
waiting parents, records one obligation and its first Propagate intent instead
of changing those runs inline. `AppWorkflows::propagation_job` applies one
bounded page: cancellation intent and Advance intents for the next children, or
wake-ups for the next idle waiting parents, committed with the obligation
cursor, page record, job receipt and next page intent. A page never defers;
exact historical pages replay after later pages advance. A notify page whose
head was restarted completes as superseded without effects. An unfinished
cascade obligation fences its source generation's cascading children:
preparation, completion and renewal treat them as cancelled, so they cannot
continue as new, and restarting one is a durable conflict until the obligation
finishes.
`runner::consumer::JobConsumer` pulls claimable jobs from the worker's
execution zone through that transport. One claimer asks for exactly its free
execution slots in one batch, continuing from the cursor the previous reply
returned, and claims again at once until a reply reaches the end of the zone
without filling them; then it waits the idle interval, or less when a slot
frees. Each slot prepares the delivery's app through
`runner::prepared::PreparedApps`, a bounded cache of app journal, executor and
residency keyed by app: a miss runs under the delivery's remaining lease,
eviction takes only idle entries, and an app the host's version feed stops
listing leaves the cache on the next claim cycle while any execution holding it
keeps it resident. A failed preparation gives the claim back, journal task
included, and withholds the app from this worker's claims until a local
expiry that doubles per consecutive failure up to a ceiling. An execution is
bounded by the smaller of the local ceiling and the delivered attempt; a
renewal that extends nothing interrupts it. A cancelled execution retains its
slot, and the app it holds, until shutdown joins. The consumer performs no
calendar evaluation, journal discovery or independent maintenance. Native tests
connect it to the ORM coordinator and queue using a separate manager database,
including a lost settlement acknowledgement.
`runner::host::WorkerHost` owns the runtime-local lifecycle for a fixed enrolled
signer: it composes the claimer, its slots and the prepared-app cache, refuses a
cache smaller than its slots, and runs once. Shutdown stops claiming and joins
execution; explicit `drain` joins retained slots. The local CLI host runs the
same consumer for its one configured app.
`zeroship-worker::workflow_creator::WorkflowCreatorFactory` assembles payload
storage, retained app artifacts, a `RemoteBackend` and the V8 executor from an
injected `WorkflowResourceProvider`. It holds no journal: its `Journal` type is
`()`, and every journal fact crosses to the workflow service. The task payload
handle and V8 backend derive from the same claimed app. Runtime metadata can
refresh env, limits, network rules and ordinary native peers, but the loader
retains the original workflow backend.
The provider must resolve independently authorized deployment-host resources;
the claimed app's id only selects them, and Control answers the worker's app
reads only for apps in the worker's own execution zone. Each prepared app holds
an `AppResidency` guard, so its key, bindings and environment stay supplied
while it or any execution from it holds the app, and only that long.
`service::reconciliation` persists a selected publication or deployment-hold page
and its progress in the journal's job receipt. It reserves each item before I/O,
so retries reach later items even when an earlier request stalls. Confirmation
checks the exact manager receipt under the original delivery and policy bounds.
Hold recovery rereads the existing durable intent and validates the matching
generation and acknowledgement; it creates no new acquisition or release intent.
Page completion commits its receipt with an app-scoped scan cursor revision and
phase. Publication completion moves to holds; hold completion returns to
publications. Captured bounds prevent publication churn from starving holds,
and competing jobs cannot move the cursor or phase backwards. Failed items remain
pending for the next bounded scan. A waiting page advances the manager's recovery deadline
only when it settles the currently recorded recovery job. Completed scans retain
periodic responsibility and provide no app-drain proof. The operation loads no
app code and examines no other app or database.
`zeroship-workflow-manager::deployments::DeploymentHolds` accepts an authorized
platform ORM database. Holds survive
reconnection, and generation checks reject stale releases. The collector helpers
run inside a host-owned transaction that also fences routing and other deployment
consumers. `service::WorkflowService` records the journal's acquisition and release
intents and reconciles them through `DeploymentHoldClient`. A release closes
deployment admission under the app lock and checks retained journal dependencies
before contacting the platform. Lost responses and host cancellation leave
durable work to retry. Delivered reconciliation uses the original manager grant
and host policy to bound pending hold recovery. The older local runner still
retries holds through its independent maintenance loop until host cutover.
Production host composition remains unfinished; the journal-reading collector
has not yet been replaced.
`OrmStore::connect` accepts the host's `DbBinding`, connection factory and keys.
The ORM owns database selection, native values and transaction settlement;
the workflow service has no separate PostgreSQL or SQLite runtime adapter.
Journal reads and writes use ORM collections and native Rust schema models.
Parity tests compare the model declarations with the migration metadata.
Restart copies retained checkpoints and
payload references through paged ORM reads and batch inserts in its transaction,
preserving effect origins and compensation metadata. Each table has an `id`
primary key; app-scoped domain keys use unique indexes and scoped foreign keys.
The engine keeps the ORM transaction callback alive until settlement; abandoning
an operation rolls it back. Database-clock reads use a separate connection to the
same journal database, so checking a lease cannot wait for the journal's own
pool lease. These clock queries read no journal state.
Descriptor installation and collection operations use the ORM's transparent
schema access, including the declared workflow tables. The native journal tests
exercise this path without a workflow-specific identifier bypass.
`schema::postgres_sql` binds the generated
DDL for a provisioning host with authorized migration credentials. Runtime
operations only verify the journal fingerprint and use ordinary DML.
PostgreSQL and SQLite use reserved `__zeroship_workflow_*` tables. The generator
compiles the canonical logical definition, then binds owned table, constraint
and index identifiers for the provisioning artifact. Table prefixes do not
restrict ORM access; schema binding and database permissions determine access.

`HostStorage` carries the app's resolved connection factory, keys and binding
to its workflow thread. Local setup initializes the journal in the
SQLite file already attached by the ORM, preserving business tables. Canonical
DDL application remains a provisioning operation. PostgreSQL runtime connections
have ordinary app-role DML permissions and cannot provision the journal.

`AppWorkflows::into_backend` creates a bounded client for other runtime threads,
including V8. Its construction site names the `WorkflowService` whose journal
that client reads and writes, so the store behind the creator seam is a choice
made there rather than one the app handle carries. The database stays on its
owning compio thread. Queue overload rejects admission; dropping a waiting call
cancels its operation and lets the ORM settle any open transaction. Callers only
receive success after confirmed commit.

`WorkflowService::open` requires `HostPolicies`. The trusted host creates a
`PolicyBinding`, reserves a refresh ticket and installs a validated snapshot:

```rust,ignore
let binding = policies.bind(app_id)?;
binding.begin_refresh()?.install(snapshot)?;
let app = service.register_app(&binding).await?;
```

App handles and queued backend calls retain that exact binding. Replacing or
revoking it invalidates old handles; neither a delayed refresh response nor
journal rows can restore their authority. Source revision and content
high water survive replacement. Configuration and leased snapshots use distinct
binding modes; changing modes requires explicit replacement. A refresh can extend
new operations, while each operation keeps its original deadline. Shortening a
lease invalidates its earlier captures permanently, even if a later refresh
extends the lease again.

Policy stays in host memory. Missing or expired authority refuses fresh mutations;
exact committed receipts and status remain scoped history reads. Explicit disabled
policy remains distinct from unavailable authority. Execution retains the original
policy capture through renewal and finalization; invalidation interrupts its
watchdog and the runner joins native work before reusing capacity. Payload reads
retain that capture through the returned body.

The workflow service is the host that binds policy in production, and it does so
on every call that reaches the journal: `RunService::app`
(`crates/zeroship-workflow-server/src/runs.rs`) observes the app's policy from
the trusted platform source, installs it as a lease snapshot whose deadline is
the observation's own validity, and carries the binding's ingress epoch forward
across that reinstall. An unchanged revision with an unmoved deadline is inert,
so a reinstall does not disturb operations already in flight. Workers hold no
policy binding and receive no policy: the service applies it to each claim,
renewal and settlement, and a renewal under policy with admission or dispatch off
extends nothing. The raw policy is shared through `zeroship_core::workflow_policy`;
source revision and lease duration remain independent. Deploy selection also
comes from the trusted host, through `activate_deploy`, without querying
platform tables.

Ordinary start, signal, broadcast, lifecycle, signal-token and signal-ingress
operations capture the host policy revision and deadline before journal I/O.
Fresh mutations recheck that authority after the app lock and before commit. The original deadline also
bounds database waits; refreshing the host snapshot cannot extend an operation
already in progress. Revocation observed before commit rolls back staged writes.
Exact committed request receipts remain replayable under their existing identity
and capability checks. These are local operation fences; distributed policy
delivery and archive quiescence still need host coordination.
Signal-token issuance and revocation retain the original token or epoch in their
request receipt. Retrying a committed revocation cannot advance its epoch again;
revocation remains available under a live policy that disables new admission.

Workflow execution reuses the app's existing deployed bundle under a durable
deployment hold. `with_deployments` binds `AppDeployments`: the normal app blob
store, a source budget and scoped retention clients supplied by the trusted host.
The [worker design](../../docs/proposals/2026-09-11-workflow-worker.md) describes
production composition and the remaining platform retention cutover.

`activate_deploy` accepts an immutable deployment registration. It acquires or
reconciles a durable hold, verifies the app-scoped manifest and its referenced
modules and descriptor, then selects the deployment under the journal's app lock.
Failed preparation preserves the previous selection. The host serializes
deployment selection updates. Once a manager activation selects a revision,
direct local activation cannot replace it. Local registration still validates
schedule declarations, but the journal owns no calendar or due cursor.
`retain_deploy` verifies repaired artifacts without changing which deployment
new runs select. Publication
and repair use the normal app deployment store.

Task executable reads resolve the deployment from a live journal claim, verify
its canonical manifest identity and content-addressed blobs, and recheck the
claim and hold after I/O. Missing or corrupt code parks that deployment;
ordinary storage outages remain retryable. Repair advances an availability
epoch so a stale failed read cannot revoke the repair. Artifact I/O releases
the app lock, allowing concurrent heartbeats and lifecycle operations. Missing
code also leaves cancellation and
expired-lease cleanup available; compensation execution still needs its code.
Manager scheduling owns due frontiers and catch-up. Creator cron acceptance
leaves missing or corrupt code retryable without consuming an occurrence.
Local host composition with the manager scheduler remains pending; the CLI's
existing task loop does not generate cron jobs.
Native contracts cover these boundaries against SQLite, PostgreSQL and S3.
The Vite plugin's `dev-bundle.ts` builds local app archives through
the deployment compiler and `.zship` packer, retaining static and dynamic module
dependencies and the captured runtime descriptor. Each build discovers fresh
declarations. The dev server publishes complete archives atomically; the CLI
ingests them into its app bundle store. HTTP and workflow execution load that
same deployment. Production worker composition and the platform retention
cutover remain unfinished.

The obsolete central task transport has been removed. The engine's
native contracts exercise app isolation, retry receipts, expired leases,
lifecycle changes, child execution, retained restart
history and scheduled occurrences through both ORM backends. PostgreSQL
fixtures use Testcontainers.
The replacement capability codecs use the platform's service signing keys.
Public signal delivery checks app and target revocation epochs transactionally;
the Control issuer and HTTP hosts are not yet wired to this replacement.
The service records task-owned payloads and promotes their references in the
completion transaction. It holds no object store: a writer, an opener and a
deleter arrive as arguments, and `zeroship-workflow-runner` supplies them over
`zeroship-storage`, verifying streamed content as it moves.
Replay generations, child results and continuation inputs retain explicit
reference edges. Collection fences uploads and retries failed deletions;
tombstones remain discoverable when an interrupted remote write arrives late.
`AppWorkflows::collect_job` accepts the manager's independent collection duty.
Each delivered page preserves a fixed observation cutoff and upper identity,
reserves item progress before deletion, and retains an exact receipt. Failed
items remain eligible for a later sweep without trapping the page's remaining
items. Deletion confirms the original tombstone fence and cannot extend a newer
collector's retention deadline. Collection keeps referenced payloads, lifecycle
history, receipts and deployment holds; completing a sweep does not certify that
the app has drained. Committed pages replay without live policy, and without
asking the deleter for anything.
App handles expose `read_step_output` for a completed
named occurrence in the run's current generation. The service resolves that
generation under the restart fence and checks reference ownership, then hands
the opener the descriptor it proved. `runner::PayloadRead::into_bytes` verifies
the stream within a host memory limit. `into_backend` adapts the app handle to
`WorkflowBackend` over the journal its caller names, refusing a service opened
over another policy registry; its bound identity cannot change between
operations. Its construction site also supplies the reader that turns a step's
recorded output into bytes, since this crate holds no store.
`AppWorkflows::with_publication_hint` tells the trusted host when a start,
signal, transition, restart, ingress acceptance or delivery completion left
pending intents, so the host can publish them at once; manager reconciliation
still recovers any intent the host misses. The hint carries no customer data.
Task hosts instead use `runner::TaskPayloadReader`: it captures the assignment's
journal, resolves named occurrences in that snapshot and reads referenced
objects through the live task lease. `WorkerTasks` implements the
payload read/write contract that the executor and its tests exercise. Hosts run
workflow work only through delivered jobs:
`zeroship serve` feeds `runner::consumer::JobConsumer` from the native manager
and has no journal polling or maintenance loop. Production worker composition
remains unfinished.
`runner::PreparedExecution` decodes runtime outcomes, leaves small values inline
and prepares task-scoped uploads for large or explicitly referenced results.
It retains upload request identities across retries, checks returned descriptors
and returns journal-ready outcomes only after uploads are confirmed. Host limits
come from `TaskPayloadLimits`, and the service enforces its app policy
independently. Their payload bounds of the same name answer to one platform
ceiling in `zeroship_core::workflow_policy`, so a payload the service admits at
staging is within the budget the host reads it back through.
An exceeded payload limit becomes a terminal failure while retaining the valid
preceding outcomes. Object references still require service ownership validation
when the task completes.
The schema check uses the built `@zeroship/migrate` and `zero-migrate-cli`
packages and their native migration addon; regenerate with
`node crates/zeroship-workflow-schema/schema/generate.mjs` from the repository root.

Run `cargo test -p zeroship-workflow` for the engine and shared service tests.
