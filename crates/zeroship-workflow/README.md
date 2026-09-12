# zeroship-workflow

The Rust workflow engine and client. This crate owns the journal protocol,
claim and apply logic, PostgreSQL journal storage, app-scoped HTTP backend,
and the local SQLite engine. It does not depend on V8 or the worker runtime.

- `engine.rs`: dispatch envelopes, outcomes, and journal folding.
- `execution.rs`: typed replay inputs, executor outcomes and runtime decoding.
- `calendar.rs`: shared cron parsing, timezone handling and calendar occurrences.
- `claim.rs`, `apply.rs`, `advance.rs`: claims, fencing, and durable advancement.
- `store/`: journal storage and its PostgreSQL implementation.
- `backend.rs`, `client.rs`: app-scoped control-plane operations for Rust hosts.
- `dev.rs`: SQLite persistence and scheduling through a host-owned `WorkflowExecutor`.
- `service/`: replacement shared service, app handles, transactional lifecycle and
  worker task protocol over PostgreSQL or SQLite.
- `schema/`: customer journal recorded through the canonical migration DSL
  and generated through its PostgreSQL and SQLite compilers.

Rust hosts can construct `HttpWorkflowBackend` with `WorkflowClientConfig` and
call `WorkflowBackend::{start,status,signal,transition,restart,read_step_output}`. The host binds
the app identity and its scoped token when constructing the backend; individual
operations cannot supply another app identity. `app_scoped_token` derives the
credential for a trusted host that already holds the control key.

The control plane authorizes these requests and owns run management. Journal
access remains subject to the database roles provisioned by the migration
service. The separate `zeroship-workflow-v8` crate installs `env.workflows`
and supplies a V8 executor for local development.

`WorkflowExecutor` accepts `WorkflowInvocation` and returns `WorkflowExecution`.
Local and deployed hosts share journal types and outcome decoding. The host
keeps lease credentials outside the invocation and binds returned outcomes to
its claim before applying them.

The replacement engine is under construction and is not yet composed into the
worker, Control or CLI. The [revised ownership design](../../docs/proposals/2026-09-11-workflow-worker.md)
embeds it in the customer's worker with customer-bound persistence; a lightweight
server coordinates metadata and does not own the journal or payloads.
`PostgresStore::new` requires an explicit validated `SchemaName`; app identity
does not select a physical schema. `schema::postgres_sql` binds the generated
DDL for a provisioning host with authorized migration credentials. Runtime
operations only verify the journal fingerprint and use ordinary DML.
PostgreSQL and SQLite use reserved `__zeroship_workflow_*` tables. The generator
compiles the canonical logical definition, then binds owned table, constraint
and index identifiers for the provisioning artifact. Creator migration and
query validators continue refusing reserved collections.

`WorkflowService::open` requires `HostPolicies`. The trusted worker supplies
validated `PolicySnapshot` values through `register_app`, then binds app code
with `for_app`. Policy stays in host memory; journal rows cannot restore
admission after a worker restart. Snapshots reject stale revisions and
conflicting limits. Explicit host configuration has no metadata expiry;
authenticated remote metadata uses a monotonic lease deadline. Expiry stops new
admission and dispatch, and heartbeat responses request a pause without
extending an existing task lease. History and completion under an already live
claim remain available. Deploy selection also comes from the trusted host,
through `activate_deploy`, without querying platform tables.

`activate_deploy` requires an `ExecutableSnapshot`: the built entry module,
dependency sources and runtime schema descriptor. `with_snapshots` binds a
`SnapshotStore` in customer object storage. Activation verifies the retained
bytes before selecting the deployment; upload failure preserves the previous
selection. A deployment identity cannot change its executable contents. The
host serializes deployment selection updates. `retain_deploy` repairs retained
code independently, preserving which deployment new runs and schedules select.

Task snapshot reads resolve the deployment from the live customer journal claim,
verify its recorded content hash and size, and recheck the lease after storage
I/O. Missing or corrupt code parks that deployment; ordinary storage outages
remain retryable. Repair advances a journal epoch so a stale failed read cannot
revoke the repair. Snapshot I/O releases the app lock, allowing concurrent
heartbeats and lifecycle operations. Missing code also leaves cancellation and
expired-lease cleanup available; compensation execution still needs its code.
Native contracts cover these boundaries against SQLite, PostgreSQL and S3.
Worker and Vite snapshot producers, snapshot collection and the worker/CLI
composition remain unfinished.

The obsolete central task transport has been removed. The engine's
native contracts exercise app isolation, retry receipts, expired leases,
lifecycle changes, child execution, retained restart
history and scheduled occurrences against both database adapters. PostgreSQL
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
