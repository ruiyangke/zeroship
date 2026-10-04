# zeroship-workflow-server

An authenticated host for native workflow coordination. Job delivery, schedules,
management and recovery persist through the shared ORM in
`zeroship-workflow-manager`. The server owns HTTP authentication, configuration,
process lifecycle and startup database-authority checks. It constructs no customer
execution engine. It does hold the payload object store: blob storage keeps large
objects out of the database, so the process that owns the journal is the one that
writes them, and the sweeps that stage and collect them are this host's own lane
to claim.

Before publishing queue dependencies, the manager records its retention intent
and obtains a deployment hold through Control. The server signs these requests
as `svc/workflow`; Control derives the app's queue holder independently from
the journal's holder. Queue retention carries no worker identity or customer
database credentials. Each HTTP thread owns its bounded Control client.

The [manager and job queue design](../../docs/proposals/2026-09-11-workflow-worker.md)
defines manager-owned cron, durable timers and the metadata job queue. The server
drives native scheduling, reconciliation, collection, retention, closing and
capacity on its own platform-bound queue.

The driver runs independently of HTTP threads and of any worker. Each pass
visits bounded pages of due schedules, independent reconciliation and collection
obligations, deployment holds and capacity targets: it resumes unfinished holds
and releases the queue hold of a deployment the app does not select once no job
needs it. A confirmed hold first stays held for the driver's hold grace, which the
server derives from `workflow.database_command_timeout_ms` so that it outlasts the
transaction that commits a dependency on it. A deadline shared by each lane's
scans and candidate operations prevents a slow candidate from consuming the next
lane's turn. Failed candidates remain durable and retry after a finite identity
sweep; restarts preserve the queue's original occurrence and recovery identities.
Shutdown stops new passes and joins the current bounded pass before releasing its
clients.

The capacity lane reads each zone's claimable backlog through the driver's own
policy source, which the server composes whether or not this process runs the
sweep lane, and asks the capacity provider for that many execution slots. Each
zone's target row also records the backlog depth, the oldest claimable row's
availability, and the creator rows that cannot deliver now: exhausted, inside a
give-back back-off, or withheld by policy or deletion. A deployment that starts
its workers itself is a static pool of `workflow.static_pool_slots` slots, and a
target beyond it is recorded as refused for an exhausted pool.

Collection duties publish code-free `Collect` jobs without a worker or deployment
hold. They remain independent of failed reconciliation and retain their pending
identity across process loss. A matching fresh `Waiting` settlement advances only
that duty; exact receipt replay cannot accelerate a later page. `Completed`
preserves periodic responsibility. Creator objects and collection cursors never
enter the manager database.

Workers authenticate with their enrolled instance key. The instance row Control
froze at join names the execution zone, and the zone is what authorizes a worker:
it claims any claimable job of its zone's apps and serves run calls for those
apps, and no worker reaches an app of another zone. A run call names an app and
is served only for an app holding a queue scope in the worker's zone, checked
before anything observes the app, so an unknown app and an app of another zone
are refused alike, with no Control read and no policy ledger row. Service
assertions use a shared PostgreSQL replay store across server replicas. Handlers authenticate
before buffering bounded JSON bodies.

Control publishes immutable, input-free deployment schedules through
`POST /v1/schedules/register` and selects their activation revision through
`POST /v1/schedules/activate`. These routes require the exact `svc/control`
signer; an instance signer cannot grant this capability. Each message names the
app's frozen zone, which the queue scope it creates records. Registration echoes
the accepted declaration, and activation returns its durable job. Replaying an
accepted older activation preserves its receipt without replacing current
schedules. An empty declaration removes future scheduling while retaining the
occurrences already accepted and their deployment holds.

`POST /v1/schedules/disable` uses that same Control authority and app revision
sequence. It records a historical receipt even before the app's first activation.
Delayed retries cannot disable a newer restore. Calendar disable preserves
accepted jobs and recovery; it does not acknowledge worker policy changes.

`POST /v1/jobs/{claim,heartbeat,settle,release,receipt}` binds worker identity
to its enrolled signing key. A claim names no app: it asks for at most its free
slots, up to `ClaimJobs::MAX_DELIVERIES`, and pages its own zone's apps from a
cursor, one job per app per lap, so a
busy app cannot starve a quiet one. It passes an app whose scope another session
holds locked rather than waiting for it, and an app that is deleted, whose policy
disables dispatch or that is at its concurrency cap. Native callbacks recheck the
exact enrolled key after the queue lock and before commit. A claimed grant the
caller cannot use is given back in the same request: a journal deferral returns
the row to `ready` until the run is due, until the observation that switched
dispatch off lapses, or after a pause that grows with each consecutive back-off,
and a grant the journal work exhausted is never handed out. Every give-back the
claim makes ends inside the caller's wait and never waits for an app's lock; one
that cannot leaves its row to lapse. A reply holds at most
`ClaimJobs::MAX_REPLY_BYTES`, room for one delivery at its largest, which is the
bound the worker accepts. Claim and heartbeat
replies transfer remaining lease and attempt durations after commit; a worker
never interprets the manager's absolute timestamps by its own wall clock. Heartbeats
never extend one attempt past `workflow.max_attempt_ms`. A committed creator
intent reaches the queue through the manager's own `Queue::submit`, inside the
process that owns the journal; no worker request path publishes one.

A settlement carries the delivery and, when the holder has one, the execution to
commit; it never carries an outcome or successors. The journal commits the
execution and the queue is settled with the outcome that commit decided. A body
with no execution is first checked against the queue's latest delivery fence for
the job, then settled from the receipt the journal already holds, and refused as
a conflict when it holds none. Exact settlement replay checks current enrollment
of the original worker. Settle and release refuse a delivery naming any worker
but the one that signed the request, and each first checks the queue's latest
delivery for the job; a release gives the row back as its reason says: after a
pause that grows with each consecutive back-off for an app that could not be
prepared, at once with the attempt counted for an interrupted attempt, and at
once with nothing counted for a delivery its holder began nothing of. A job's receipt is read only by the worker the queue
last delivered it to. The task routes carry no delivery, so they are served only
to a worker holding a live delivery of the app they name. Every one of those
checks comes before any journal is asked.

Control accepts lifecycle commands through `POST /v1/management/enqueue` and
reads their durable outcome through `POST /v1/management/status`. Commands become
ordered `Management` jobs in the same queue as execution work. The service's own
maintenance lane claims and applies them and settles each with the outcome its
journal committed; there is no separate command polling or acknowledgement route.
Acceptance atomically records the command, job, run ordering and execution
barrier. Settlement commits the queue receipt, command outcome and matching
barrier change together.

Latest restart names its deployment in the command. Control is the authority for
the app pointer and the deployment catalog and is the only caller this endpoint
admits, so the server resolves nothing: it obtains queue retention for the named
deployment and freezes that identity in the accepted job. The named pair is
checked against the hold Control minted for it, and a hash that differs is a
conflict.

`WorkflowHttpState::policy_source` injects the trusted platform provider. The
binary installs native `ControlPolicies` against its existing platform database,
reading each app's policy inputs, zone and deletion marker over Control's
app-facts endpoint. The provider serializes revision publication before reading
app, plan and operator inputs together. Cache hits preserve original source
validity; expired, missing or malformed authority produces a retryable
infrastructure failure. Operators provision complete plan policy and the rollout
validity explicitly. The service updates its policy ledger; it cannot update app
or plan inputs or access creator storage.

The platform migration creates `workflow_manager` metadata under a migration
owner and grants the `zeroship_workflow` login ordinary DML. Runtime verifies the
schema fingerprint and rejects elevated roles, role memberships, DDL and
mutable schema fingerprints. It can read the enrolled worker instance columns its
authentication projects and maintain service-assertion receipts, and it holds no
grant on `zeroship.apps` or on a creator table. Loss of the shared authentication
connection stops the process for supervisor recovery instead of leaving a
listener attached to a dead verifier connection.

- `src/api.rs`, `src/api/`: the closed metadata HTTP operations.
- `src/auth.rs`: service assertions and enrolled worker key and zone verification.
- `src/coordinator.rs`: provisioned ORM composition and startup authority checks.
- `../zeroship-workflow-manager/src/coordinator/`: native zone claims and management.
- `src/config.rs`: `[workflow]` settings and generated CLI overrides.
- `src/payloads.rs`: the payload object store and the byte capabilities the sweeps take.
- `src/sweeps.rs`: this process's own lane over the journal sweeps of its queue.
- `src/server.rs`: metadata pools, verification, native driver and HTTP lifecycle.
- `../zeroship-workflow-manager/schema/schema.ts`: the shared migration DSL definition.
- `tests/integration/coordinator.rs`: native store, fencing and startup contracts.
- `tests/integration/http_claims.rs`: zone claims, give-backs and leases through real authentication.
- `tests/integration/http_runs.rs`: run calls authorized by the worker's zone.
- `tests/integration/control_policy.rs`: canonical Control migrations, source-role isolation, publication ordering and bounded caching.
- `tests/integration/maintenance_lane.rs`: the lane's claims, its staged objects and its bounded turns.
- `tests/integration/platform_schema.rs`: actual platform migrations and database authority.
- `tests/e2e/http.rs`: real server processes, replicas, revocation and restart.
- `tests/e2e/http_jobs.rs`: job delivery, holder authority and enrollment changes during lock waits.
- `tests/e2e/http_schedules.rs`: Control-only publication, immutable replies and schedule replacement without workers.
- `tests/e2e/driver.rs`: process-owned scheduling, collection, retention and capacity without workers.

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
by default. The host needs no customer database connection. It does need
`workflow.storage_url`, and it must name the same object store the deployment's
workers name: this service writes a run's staged input and an executing run reads
that object back, so a store only one of them can reach is a run that cannot
start. Startup refuses an absent or unusable location rather than claiming sweeps
it could only fail.
`workflow.delivery_lease_ms` is how long a claimed delivery stays leased without
a heartbeat, and `workflow.max_attempt_ms`, at least that long, caps one attempt
across heartbeats; `workflow.claim_budget_ms` bounds the server's work inside the
wait a claim states. `workflow.driver_interval_ms` controls the delay after a
completed pass; `workflow.driver_lane_timeout_ms` bounds each lane.
`workflow.batch_limit` also bounds the candidate page. `workflow.maintenance_sweeps`
decides whether this process composes its own sweep lane at all; it is on, and a
host that sets it off composes none, for a queue another process is the sweep
authority over. The closing lane begins closing an app's recovery responsibility
after `workflow.closing_idle_ms` without activity, or once Control archived the
app; `workflow.closing_timeout_ms` bounds an attempt, and
`workflow.closing_backoff_ms` doubles up to `workflow.closing_backoff_max_ms`
between attempts that did not retire. The lane abandons an app Control deleted
instead, reading only the deletion marker, and settles that app's unsettled
creator jobs `Rejected`. These settings control the manager
host and add no workflow-specific creator CLI setup.
`zeroship-workflow-server --config zeroship.toml --check-config`
validates settings without connecting to dependencies. `/healthz` reports process
liveness; `/readyz` verifies metadata and worker-registry access.
