# zeroship-workflow-server

An authenticated host for native workflow coordination. Worker registration,
app placement, job delivery, wake hints and management persist through the shared ORM in
`zeroship-workflow-manager`. The server owns HTTP authentication, configuration,
process lifecycle and startup database-authority checks. It constructs no customer
execution engine or payload store.

Before publishing queue dependencies, the manager records its retention intent
and obtains a deployment hold through Control. The server signs these requests
as `svc/workflow`; Control derives the app's queue holder independently from
worker journal holders. Queue retention carries no worker assignment or customer
database credentials. Each HTTP thread owns its bounded Control client.

The [manager and job queue design](../../docs/proposals/2026-09-11-workflow-worker.md)
defines manager-owned cron, durable timers and the metadata job queue. Native
queue and placement operations share the manager's database and transaction
handle. The server drives native scheduling, reconciliation, collection and
retention on its own platform-bound queue. Normal deployment publication and the
worker consumer still require cutover. Existing workers still discover due
customer work; that behavior does not define the target role split.

The driver runs independently of HTTP threads and worker registration. Each pass
visits bounded pages of due schedules, independent reconciliation and collection
obligations, and deployment holds: it resumes unfinished holds and releases the
queue hold of a deployment the app no longer selects once no job needs it. A
confirmed hold first stays held for the driver's hold grace, which the server
derives from `workflow.database_command_timeout_ms` so that it outlasts the
transaction that commits a dependency on it. A deadline shared by each lane's
scans and candidate operations prevents a slow candidate from consuming the next
lane's turn. Failed candidates
remain durable and retry after a finite identity sweep; restarts preserve the
queue's original occurrence and recovery identities. Shutdown stops new passes
and joins the current bounded pass before releasing its clients.

Collection duties publish code-free `Collect` jobs without a worker or deployment
hold. They remain independent of failed reconciliation and retain their pending
identity across process loss. A matching fresh `Waiting` settlement advances only
that duty; exact receipt replay cannot accelerate a later page. `Completed`
preserves periodic responsibility. Creator objects and collection cursors never
enter the manager database.

Control authorizes placement and queues typed pause, resume, cancellation or
restart commands. Workers authenticate with their enrolled instance key, then
register, discover assignments, renew placement, publish hints and acknowledge
management. Every worker mutation checks app, worker, assignment revision and
expiry. Registration cannot nominate an app or revive an expired placement.
Service assertions use a shared PostgreSQL replay store across server replicas.
Handlers authenticate before buffering bounded JSON bodies.

Control publishes immutable, input-free deployment schedules through
`POST /v1/schedules/register` and selects their activation revision through
`POST /v1/schedules/activate`. These routes require the exact `svc/control`
signer; an instance signer or worker assignment cannot grant this capability.
Registration echoes the accepted declaration, and activation returns its durable
job. Replaying an accepted older activation preserves its receipt without
replacing current schedules. An empty declaration removes future scheduling
while retaining previously accepted occurrences and their deployment holds.
The normal deploy transaction still needs its durable publication handoff.

`POST /v1/schedules/disable` uses that same Control authority and app revision
sequence. It records a historical receipt even before the app's first activation.
Delayed retries cannot disable a newer restore. Calendar disable preserves
accepted jobs and recovery; it does not acknowledge worker policy changes.

`POST /v1/jobs/{submit,claim,heartbeat,settle}` binds worker identity to its
enrolled signing key. Native callbacks recheck that exact key and stored app
placement after queue locks and before commit. Workers cannot submit manager-owned
cron or management commands. Exact settlement replay checks current enrollment
of the original worker even after placement expires. Claim and heartbeat replies
transfer remaining lease duration after commit; worker wall clocks are not used
to interpret the manager's absolute timestamps.

Control accepts lifecycle commands through `POST /v1/management/enqueue` and
reads their durable outcome through `POST /v1/management/status`. Commands become
ordered `Management` jobs in the same queue as execution work. Workers claim and
settle them through the job endpoints with a closed management outcome; there is
no separate command polling or acknowledgement route. Acceptance atomically
records the command, job, run ordering and execution barrier. Settlement commits
the queue receipt, command outcome and matching barrier change together. Exact
settlement replay preserves that result after placement replacement while still
checking the original worker's enrollment.

Latest restart names its deployment in the command. Control is the authority for
the app pointer and the deployment catalog and is the only caller this endpoint
admits, so the server resolves nothing: it obtains queue retention for the named
deployment and freezes that identity in the accepted job. The named pair is
checked against the hold Control minted for it, and a hash that differs is a
conflict. Creator-side management delivery remains pending; the server performs
no customer lifecycle mutation or restart execution.

`POST /v1/policy/lease` accepts only an `AssignedScope` from an enrolled worker.
The response binds complete policy to that app, worker, signing-key thumbprint
and assignment revision. The native manager validates assignment before source
I/O and rechecks authority after waits without renewing registration or placement.
`WorkflowHttpState::policy_source` injects the trusted platform provider. The
binary installs native `ControlPolicies` against its existing platform database.
The provider serializes revision publication before reading app, plan and
operator inputs together. Cache hits preserve original source validity; expired,
missing or malformed authority produces a retryable infrastructure failure.
Operators provision complete plan policy and the rollout validity explicitly.
The service can read those Control columns and update its policy ledger; it
cannot update app or plan inputs or access creator storage.

Assignments and mutation receipts survive restart. Wake revisions reject stale
or conflicting publication. A worker cannot release the last active placement;
missing owners expose the app for host-driven recovery and a customer-journal
rescan, even when the previous worker never published a hint. Creator task claims
and management application remain transactions in the customer's database.

The platform migration creates `workflow_manager` metadata under a migration
owner and grants the `zeroship_workflow` login ordinary DML. Runtime verifies the
schema fingerprint and rejects elevated roles, role memberships, DDL and
mutable schema fingerprints. It can read enrolled worker verification keys and
maintain service-assertion receipts, with no journal or customer-table grants.
Loss of the shared authentication connection stops the process for supervisor
recovery instead of leaving a listener attached to a dead verifier connection.

- `src/api.rs`: the closed metadata HTTP operations.
- `src/auth.rs`: service assertions and enrolled worker key verification.
- `src/coordinator.rs`: provisioned ORM composition and startup authority checks.
- `../zeroship-workflow-manager/src/coordinator/`: native placement and management.
- `src/config.rs`: `[workflow]` settings and generated CLI overrides.
- `src/server.rs`: metadata pools, verification, native driver and HTTP lifecycle.
- `../zeroship-workflow-manager/schema/schema.ts`: the shared migration DSL definition.
- `tests/coordinator.rs`: native store, fencing and recovery contracts.
- `tests/http.rs`: real server processes, replicas, revocation and restart.
- `tests/http_jobs.rs`: job delivery, scoped publication and enrollment changes during lock waits.
- `tests/http_policy.rs`: assignment-scoped policy, source failures and enrolled-key replacement during issuance.
- `tests/control_policy.rs`: canonical Control migrations, source-role isolation, publication ordering and bounded caching.
- `tests/http_schedules.rs`: Control-only publication, immutable replies and schedule replacement without workers.
- `tests/driver.rs`: process-owned scheduling, collection and retention recovery without workers.
- `tests/platform_schema.rs`: actual platform migrations and database authority.

Run `cargo test -p zeroship-workflow-server` for the host contracts. Required
PostgreSQL fixtures are owned by Testcontainers. `cargo xtask test workflow`
includes the coordinator alongside the engine and example suites.
Regenerate metadata SQL with
`node crates/zeroship-workflow-manager/schema/generate.mjs`.

The host needs a platform metadata database login, `workflow.control_url`, its
private `workflow.service_key_file`, and `workflow.service_peers_file` containing
Control's public key. Control's peer bundle must contain the workflow service's
public key. The Control origin requires HTTPS, except for literal
loopback addresses and exact origins named in `plaintext_peers`, which is empty
by default. The host needs no customer connection or payload location.
`workflow.driver_interval_ms` controls the delay after a completed pass;
`workflow.driver_lane_timeout_ms` bounds each lane. `workflow.batch_limit` also
bounds the candidate page. The closing lane begins closing an app's recovery
responsibility after `workflow.closing_idle_ms` without activity, or once
Control archived the app; `workflow.closing_timeout_ms` bounds an attempt, and
`workflow.closing_backoff_ms` doubles up to `workflow.closing_backoff_max_ms`
between attempts that did not retire. The lane abandons an app Control deleted
instead, reading only the deletion marker. These settings control the manager
host and add no workflow-specific creator CLI setup.
`zeroship-workflow-server --config zeroship.toml --check-config`
validates settings without connecting to dependencies. `/healthz` reports process
liveness; `/readyz` verifies metadata and worker-registry access.
