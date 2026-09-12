# zeroship-workflow

The Rust workflow engine and client. This crate owns the journal protocol,
claim and apply logic, PostgreSQL journal storage, app-scoped HTTP backend,
and the local SQLite engine. It does not depend on V8 or the worker runtime.

- `engine.rs`: dispatch envelopes, outcomes, and journal folding.
- `claim.rs`, `apply.rs`, `advance.rs`: claims, fencing, and durable advancement.
- `store/`: journal storage and its PostgreSQL implementation.
- `backend.rs`, `client.rs`: app-scoped control-plane operations for Rust hosts.
- `dev.rs`: SQLite persistence and scheduling through a host-owned `WorkflowExecutor`.

Rust hosts can construct `HttpWorkflowBackend` with `WorkflowClientConfig` and
call `WorkflowBackend::{start,status,signal,transition,restart,read_step_output}`. The host binds
the app identity and its scoped token when constructing the backend; individual
operations cannot supply another app identity. `app_scoped_token` derives the
credential for a trusted host that already holds the control key.

The control plane authorizes these requests and owns run management. Journal
access remains subject to the database roles provisioned by the migration
service. The separate `zeroship-workflow-v8` crate installs `env.workflows`
and supplies a V8 executor for local development.

Run `cargo test -p zeroship-workflow` for the engine and SQLite journal tests.
