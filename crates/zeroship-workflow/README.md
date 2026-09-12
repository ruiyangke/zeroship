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
- `schema/`: owned service schema recorded through the canonical migration DSL
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

The replacement service is under construction and is not yet composed into the
worker, Control or CLI. Its native contracts exercise app isolation, retry
receipts, expired leases, lifecycle changes, child execution, retained restart
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
Embedded and remote app handles expose `read_step_output` for a completed
named occurrence in the run's current generation. The service resolves that
generation under the restart fence and checks reference ownership before
opening storage. `PayloadRead::into_bytes` verifies the stream within a host
memory limit. `into_backend` adapts either app handle to `WorkflowBackend`;
its bound identity cannot change between operations.
The schema check uses the built `@zeroship/migrate` and `zero-migrate-cli`
packages and their native migration addon; regenerate with
`node crates/zeroship-workflow/schema/generate.mjs` from the repository root.

Run `cargo test -p zeroship-workflow` for the engine and shared service tests.
