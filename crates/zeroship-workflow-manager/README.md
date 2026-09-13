# zeroship-workflow-manager

Native platform workflow coordination. This crate owns worker registration,
placement, calendar scheduling, management commands, the durable metadata queue and the deployment
retention ledger through the shared Rust ORM. It has no customer journal,
payload storage or V8 dependency.

`coordinator::Coordinator` uses the queue's bound database and app-scope lock.
Placement changes lock the app scope before the worker row so capacity remains
serialized across apps. Native job operations verify stored assignments and
invoke the host's current enrollment check inside the queue transaction.
Recovery pagination filters existing owners before
ending a result page, so owned scopes cannot hide later recovery work.
Coordinator reads use generated field predicates and native `FromRow` decoding;
the queue's app locks and conditional writes remain the serialization boundary.

`Queue` binds a provisioned platform database. App locks serialize queue changes;
delivery attempts fence retries and stale acknowledgements. Settlement writes
its receipt and successor jobs in the same transaction. Authenticated hosts
revalidate placement through the authorized methods after lock waits and before
commit. Receipt replay requires the original worker's current enrollment.
Timeout during an in-flight commit leaves an uncertain outcome; retries recover
its durable receipt without adding successor jobs again.
Claim and heartbeat return `DeliveryGrant`; converting it to a wire lease after
commit charges elapsed time against the originally observed assignment authority.
Worker publication cannot mint manager-owned activation, cron or management commands.

`scheduling::Scheduler` prepares immutable schedule metadata from normal app
deployments. Activation selects future scheduling and commits its job and recovery
responsibility together. Due dispatch persists occurrence identities, queue jobs
and the catch-up cursor in one transaction. Page limits preserve the original
catch-up boundary and remaining allowance. Occurrences wait for their own
activation job to complete; replacement stops future generation without changing
already queued jobs. Claims and receipt replay verify stored job linkage, and
frontier extension checks the immutable descriptor and calendar interpretation.
Creator activation and cron acceptance are handled by the customer engine.
The server drives due scheduling through the native manager. Normal deployment
publication and ordinary worker/CLI composition still need integration.

Calendar disable shares the platform's activation revision sequence. Its durable
receipt remains replayable after restore, and disabling before the first
activation fences delayed older requests. Disable retains accepted jobs, calendar
progress, recovery responsibility and holds. Restoring the same deployment
preserves its interval anchor and catch-up progress while creating new activation
readiness. Due scans exclude disabled scopes before applying their page limits.
This gate controls calendar publication; creator admission and executor shutdown
have separate policy authority.

`driver::Driver` visits bounded calendar, recovery and unfinished-hold pages.
Each lane captures its upper identity and advances past an attempted candidate
before I/O. A shared lane deadline bounds scans and candidate operations; failed
items remain durable and retry after the finite sweep wraps. Cancellation retains
scan progress and rotates the next lane. The host owns cadence and shutdown.
The driver needs no worker registration, placement or creator database. Retention
processing resumes acquiring/releasing intents; releasing a held deployment still
requires the host's explicit release operation and its dependency checks.

`recovery::Recovery` stores persistent scope responsibility and publishes due
reconciliation into this queue. Repeated activation registration preserves its
deadline; newer activation affects future jobs while a pending job keeps its pin.
Publication and the next deadline commit together. An unsettled job is reused
across replicas and restarts, so absent workers cannot erase responsibility or
accumulate replacement jobs. Bounded due pages include healthy scopes and must
be swept repeatedly. Host scheduling, capacity provisioning and the ingress
epoch handshake still require integration before enabling customer ingress.

`deployments::DeploymentHolds` belongs in Control or the local platform host.
It records normal app deployments and generation-fenced retention holds. Manager
queue ownership and customer journal ownership require separate host authority;
the journal hold API does not grant manager access.

`retention::HoldClient` supplies the queue's Control capability. Queue publication,
activation and recovery require a confirmed deployment hold. The queue persists
acquire/release intents and generations; network requests run outside its database
transactions. Release closes publication and checks pending jobs, schedule
frontiers and recovery responsibility under the app lock. Completed receipts may
replay after code reclamation. A host must reconcile unfinished intents, and
Control reclamation must consult the shared ledger before deleting manifests.

`schema/` and `schema/deployments/` record definitions through the migration DSL
and generate migration metadata and dialect DDL. Rust ORM models use native
`schema!` declarations, checked against that metadata in tests. Runtime queue and
retention operations use collections and models. Database clock queries and local catalog
provisioning remain explicit host operations.

The native library is composed by the workflow server under
the [workflow proposal](../../docs/proposals/2026-09-11-workflow-worker.md).
Normal deployment publication, the worker consumer and CLI composition still require cutover.

Run `cargo test -p zeroship-workflow-manager` for PostgreSQL Testcontainers and
SQLite queue, scheduling and retention contracts.
