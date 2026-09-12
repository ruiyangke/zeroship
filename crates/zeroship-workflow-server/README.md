# zeroship-workflow-server

A lightweight coordinator for worker registration, app placement, wake-up hints
and high-level workflow management. Customer workers own execution, task leases,
scheduling, history and payload storage. The server does not construct an
execution engine or payload store. Its HTTP contract contains metadata operations
and exposes no task-completion, input, signal-body or output-upload endpoint.

The [ownership design](../../docs/proposals/2026-09-11-workflow-worker.md) describes
the complete target. The metadata HTTP host and platform migration implement this
boundary; composing the replacement engine into the customer worker and CLI is
still in progress.

Control authorizes placement and queues typed pause, resume, cancellation or
restart commands. Workers authenticate with their enrolled instance key, then
register, discover assignments, renew placement, publish hints and acknowledge
management. Every worker mutation checks app, worker, assignment revision and
expiry. Registration cannot nominate an app or revive an expired placement.
Service assertions use a shared PostgreSQL replay store across server replicas.
Handlers authenticate before buffering bounded JSON bodies.

Assignments and mutation receipts survive restart. Wake revisions reject stale
or conflicting publication. A worker cannot release the last active placement;
missing owners expose the app for host-driven recovery and a customer-journal
rescan, even when the previous worker never published a hint. Actual task claims
and management application remain transactions in the customer's database.

The platform migration creates `workflow_coordination` metadata under a migration
owner and grants the `zeroship_workflow` login ordinary DML. Runtime verifies the
schema fingerprint and rejects elevated roles, role memberships, DDL and
mutable schema fingerprints. It can read enrolled worker verification keys and
maintain service-assertion receipts, with no journal or customer-table grants.
Loss of the shared authentication connection stops the process for supervisor
recovery instead of leaving a listener attached to a dead verifier connection.

- `src/api.rs`: the closed metadata HTTP operations.
- `src/auth.rs`: service assertions and enrolled worker key verification.
- `src/coordinator/`: transactional placement and management persistence.
- `src/config.rs`: `[workflow]` settings and generated CLI overrides.
- `src/server.rs`: metadata pools, verification and HTTP lifecycle.
- `schema/schema.ts`: the canonical migration DSL definition.
- `tests/coordinator.rs`: native store, fencing and recovery contracts.
- `tests/http.rs`: real server processes, replicas, revocation and restart.
- `tests/platform_schema.rs`: actual platform migrations and database authority.

Run `cargo test -p zeroship-workflow-server` for the host contracts. Required
PostgreSQL fixtures are owned by Testcontainers. `cargo xtask test workflow`
includes the coordinator alongside the engine and example suites.
Regenerate metadata SQL with
`node crates/zeroship-workflow-server/schema/generate.mjs`.

The host needs a platform metadata database login and a peer verification bundle
containing Control's key. It needs no customer connection, payload location or
private signing key. `zeroship-workflow-server --config zeroship.toml --check-config`
validates settings without connecting to dependencies. `/healthz` reports process
liveness; `/readyz` verifies metadata and worker-registry access.
