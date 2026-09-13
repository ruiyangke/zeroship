# zeroship-workflow-manager

Native platform workflow coordination. This crate owns worker registration,
placement, management commands, the durable metadata queue and the deployment
retention ledger through the shared Rust ORM. It has no customer journal,
payload storage or V8 dependency.

`coordinator::Coordinator` uses the queue's bound database and app-scope lock.
Placement changes lock the app scope before the worker row so capacity remains
serialized across apps. Native queue claims verify stored assignments inside
the queue transaction. Recovery pagination filters existing owners before
ending a result page, so owned scopes cannot hide later recovery work.

`Queue` binds a provisioned platform database. App locks serialize queue changes;
delivery attempts fence retries and stale acknowledgements. Settlement writes
its receipt and successor jobs in the same transaction. Authenticated hosts
revalidate placement through the authorized methods after lock waits and before
commit. Receipt replay requires the original worker's current enrollment.
Timeout during an in-flight commit leaves an uncertain outcome; retries recover
its durable receipt without adding successor jobs again.
Scope registration establishes queue storage; manager-owned reconciliation
deadlines must still be integrated before enabling customer ingress.

`deployments::DeploymentHolds` belongs in Control or the local platform host.
It records normal app deployments and generation-fenced retention holds. Manager
queue ownership and customer journal ownership require separate host authority;
the journal hold API does not grant manager access.

`schema/` and `schema/deployments/` record definitions through the migration DSL
and generate native ORM descriptors and dialect DDL. Runtime queue and retention
operations use collections and models. Database clock queries and local catalog
provisioning remain explicit host operations.

The native library is being integrated into the workflow server and CLI under
the [workflow proposal](../../docs/proposals/2026-09-11-workflow-worker.md).
Production queue endpoints and the worker scheduling loop still require cutover.

Run `cargo test -p zeroship-workflow-manager` for PostgreSQL Testcontainers and
SQLite queue and retention contracts.
