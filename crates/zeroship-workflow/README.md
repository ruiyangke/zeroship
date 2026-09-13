# zeroship-workflow

The Rust customer workflow engine. This crate owns the journal protocol,
claim and apply logic, PostgreSQL journal storage, app-scoped HTTP backend,
and customer-bound PostgreSQL and SQLite execution. It does not depend on V8.

- `engine.rs`: dispatch envelopes, outcomes, and journal folding.
- `execution.rs`: typed replay inputs, executor outcomes and runtime decoding.
- `zeroship-workflow-calendar`: shared parsing and timing semantics used by the
  manager; creator acceptance does not evaluate calendars.
- `claim.rs`, `apply.rs`, `advance.rs`: claims, fencing, and durable advancement.
- `store/`: existing Control journal implementation, awaiting production cutover.
- `backend.rs`, `client.rs`: app-scoped control-plane operations for Rust hosts.
- `service/`: replacement shared service, app handles, transactional lifecycle and
  worker task protocol through the shared Rust ORM.
- `schema/`: customer journal recorded through the canonical migration DSL
  and generated through its PostgreSQL and SQLite compilers.
- `deployment_holds/`: scoped customer clients for deployment retention.
  `zeroship-workflow-manager::deployments` owns the platform ORM ledger and its
  canonical schema; `zeroship-workflow-client` owns authenticated transport.

Rust hosts can construct `HttpWorkflowBackend` with `WorkflowClientConfig` and
call `WorkflowBackend::{start,status,signal,transition,restart,read_step_output}`. The host binds
the app identity and its scoped token when constructing the backend; individual
operations cannot supply another app identity. `app_scoped_token` derives the
credential for a trusted host that already holds the control key.

The control plane authorizes these requests and owns run management. Journal
access remains subject to the database roles provisioned by the migration
service. The separate `zeroship-workflow-v8` crate installs `env.workflows`
and supplies a V8 executor for local development.

The shared runner supplies `WorkflowInvocation` and receives `WorkflowExecution`.
Local and deployed hosts share journal types and outcome decoding. The host
keeps lease credentials outside the invocation and binds returned outcomes to
its claim before applying them.

The shared engine is composed into the CLI; production worker and Control
integration remain unfinished. The [revised design](../../docs/proposals/2026-09-11-workflow-worker.md)
assigns scheduling and queue delivery to the manager, with workers consuming
bounded jobs against creator storage. The [planned crate layout](../../docs/proposals/2026-09-11-workflow-worker.md#crate-layout)
keeps the manager and service client separate from customer execution here.
Their native crates exist; production scheduling and delivery still await
cutover. Workers may access only creator databases; Control and other
platform services may access only the Control database. Authenticated service
contracts carry cross-boundary requests without sharing database credentials.
`AppWorkflows::apply_management` commits coordinator lifecycle commands with
durable customer-journal receipts. Repeated commands return their recorded
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
`service::publication` records immutable advance-job intents with creator
transitions. `AppWorkflows::pending_jobs` and `publish_job` publish under a bound
app scope without holding a creator transaction across manager I/O. The entire
returned specification must match before confirmation; retries preserve job
identities, and pending intents retain deployment dependencies independently of
run history. `AssignedPublisher` uses the authenticated worker client.
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
`runner::delivery::DeliverySlot` routes activation, cron and reconciliation jobs to
bounded journal operations and advance jobs to the existing executor and
payload pipeline. Its host supplies `JobTransport`;
the authenticated worker client implements that metadata interface. The slot
renews manager and creator authority together, retains interrupted execution
until native shutdown joins, and retries exact settlement after a committed
creator result. This slot performs no journal discovery or calendar evaluation.
`runner::consumer::JobConsumer` claims manager jobs through that transport and
shares bounded execution capacity across trusted `ConsumerScope` bindings. Each
binding pairs an app handle with its own executor and creator storage. The host
replaces the authorized snapshot through `ConsumerBindings`; unchanged binding
clones preserve active work, while replacement or removal cancels the previous
binding. A cancelled execution retains its slot until shutdown joins. Selection
rotates between eligible apps and bounds claim I/O, idle polling and error retries.
The consumer performs no calendar evaluation, journal discovery or independent
maintenance. Native tests connect it to the ORM coordinator and queue using a
separate manager database, including a lost settlement acknowledgement.
`runner::assignments::AssignmentBindings` connects authenticated manager placement
to those bindings. It reads through an empty assignment page before changing the
snapshot, preserves unchanged policy generations, and retires removals before
opening replacement creator resources. `CreatorFactory` resolves resources from
trusted host configuration and must return the exact supplied policy binding.
Renewal, policy refresh and preparation progress independently across apps under
operation bounds and original policy deadlines. Closing the reconciler revokes
local authority synchronously; the host separately joins consumer execution.
`runner::host::WorkerHost` owns the runtime-local lifecycle for a fixed enrolled
signer. It registers before scanning, then drives registration, assignment scans,
policy renewal and consumption independently. App-placement capacity follows the
assignment bound; execution slots are a separate local limit. Transient manager
outages retry without extending authority. Identity refusal or shutdown cancels
pending refreshes, revokes local bindings and announces terminal draining while
joining execution. Cancellation also retires the host; explicit `drain` joins
retained slots, and the same host cannot become ready again. The host does not
release assignments or discharge manager recovery responsibility.
Production enrollment, trusted creator resource providers, remaining delivered
operation handlers and ordinary worker/CLI composition remain required before
replacing the existing runner.
`service::reconciliation` persists the selected publication page and its progress
in the creator job receipt. It reserves each item before attempting publication,
so retries reach later items even when an earlier request stalls. Confirmation
checks the exact manager receipt under the original delivery and policy bounds.
Page completion commits its receipt with an app-scoped scan cursor revision;
competing jobs cannot move that cursor backwards. Failed items remain pending for
the next bounded scan. A waiting page advances the manager's recovery deadline
only when it settles the currently recorded recovery job. Completed scans retain
periodic responsibility and provide no app-drain proof. The operation loads no
app code and examines no other app or database.
`zeroship-workflow-manager::deployments::DeploymentHolds` accepts an authorized
platform ORM database. Holds survive
reconnection, and generation checks reject stale releases. The collector helpers
run inside a host-owned transaction that also fences routing and other deployment
consumers. `service::WorkflowService` records customer-side acquisition and release
intents and reconciles them through `DeploymentHoldClient`. A release closes
deployment admission under the app lock and checks retained journal dependencies
before contacting the platform. Lost responses and host cancellation leave
durable work to retry. The worker automatically reconciles pending holds through
its host-bound clients, including after restart or policy-lease expiry. Recovery
rotates between assigned apps and advances past failed intents; individual calls
are bounded by the maintenance timeout. Execution and maintenance use independent
wake queue entries so busy polling cannot starve recovery. Production host
composition remains unfinished; the journal-reading collector has not yet been replaced.
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
same customer database, so checking a lease cannot wait for the journal's own
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

`HostStorage` carries the app's resolved connection factory, keys, binding and
object store to its workflow thread. Local setup initializes the journal in the
SQLite file already attached by the ORM, preserving business tables. Canonical
DDL application remains a provisioning operation. PostgreSQL runtime connections
have ordinary app-role DML permissions and cannot provision the journal.

`AppWorkflows::into_backend` creates a bounded client for other runtime threads,
including V8. The database stays on its owning compio thread. Queue overload
rejects admission; dropping a waiting call cancels its operation and lets the ORM
settle any open transaction. Callers only receive success after confirmed commit.

`WorkflowService::open` requires `HostPolicies`. The trusted host creates a
`PolicyBinding`, reserves a refresh ticket and installs a validated snapshot:

```rust,ignore
let binding = policies.bind(app_id)?;
binding.begin_refresh()?.install(snapshot)?;
let app = service.register_app(&binding).await?;
```

App handles and queued backend calls retain that exact binding. Replacing or
revoking it invalidates old handles; neither a delayed refresh response nor
customer journal rows can restore their authority. Source revision and content
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

For an assigned remote host, `AssignedPolicies` binds a fixed worker client and
assignment to a new policy generation:

```rust,ignore
let remote = AssignedPolicies::new(&policies, worker_client, assignment)?;
remote.refresh().await?;
let app = service.register_app(remote.binding()).await?;
```

Keep this handle for the unchanged association. Constructing a replacement
retires old handles, while cloning preserves their generation. Each refresh
reserves its ticket before HTTP and installs the validated client's original
monotonic deadline. Delayed replies cannot replace newer refreshes or a new
binding. Failed exchanges leave previous authority bounded by its existing
deadline. The raw policy is shared through `zeroship_core::workflow_policy`;
source revision and lease duration remain independent. The authoritative Control
source and ordinary production assignment/refresh loop still require integration.
Deploy selection also comes from the trusted host, through `activate_deploy`,
without querying platform tables.

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
modules and descriptor, then selects the deployment under the customer app lock.
Failed preparation preserves the previous selection. The host serializes
deployment selection updates. Once a manager activation selects a revision,
direct local activation cannot replace it. Local registration still validates
schedule declarations, but the creator journal owns no calendar or due cursor.
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
The service stages task-owned payloads through `zeroship-storage`, verifies
streamed content and promotes references in the completion transaction.
Replay generations, child results and continuation inputs retain explicit
reference edges. Collection fences uploads and retries failed deletions;
tombstones remain discoverable when an interrupted remote write arrives late.
Payload contracts run against local storage and Testcontainers S3.
App handles expose `read_step_output` for a completed
named occurrence in the run's current generation. The service resolves that
generation under the restart fence and checks reference ownership before
opening storage. `PayloadRead::into_bytes` verifies the stream within a host
memory limit. `into_backend` adapts the app handle to `WorkflowBackend`;
its bound identity cannot change between operations.
Task hosts instead use `runner::TaskPayloadReader`: it captures the assignment's
journal, resolves named occurrences in that snapshot and reads referenced
objects through the live task lease. `WorkerTasks` implements the
payload read/write contract alongside the local task protocol.
`runner::WorkflowWorker` drives bounded task slots and expired
payload collection on its host's compio thread. Background discovery selects
only host-assigned apps before applying batch limits; customer journal rows
cannot register apps with the host. Expired assignments still permit lease and
payload cleanup, while current policy is checked again before mutation.
The loop keeps working without a
request isolate, retries failed attempts with a delay, and bounds task polling
and maintenance I/O. Shutdown cancels active execution and waits for it to stop
before releasing claims. A cancelled host future must be drained before its
slots are discarded or reused. `WorkerOptions` supplies the host limits, and
construction requires customer payload and executable storage. The CLI starts
this loop on a dedicated thread and supplies a persisted project identity.
Production worker composition remains unfinished.
`runner::PreparedExecution` decodes runtime outcomes, leaves small values inline
and prepares task-scoped uploads for large or explicitly referenced results.
It retains upload request identities across retries, checks returned descriptors
and returns journal-ready outcomes only after uploads are confirmed. Host limits
come from `TaskPayloadLimits`; the service enforces its app policy independently.
An exceeded payload limit becomes a terminal failure while retaining the valid
preceding outcomes. Object references still require service ownership validation
when the task completes.
The schema check uses the built `@zeroship/migrate` and `zero-migrate-cli`
packages and their native migration addon; regenerate with
`node crates/zeroship-workflow/schema/generate.mjs` from the repository root.

Run `cargo test -p zeroship-workflow` for the engine and shared service tests.
