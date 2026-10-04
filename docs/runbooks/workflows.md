# Durable Workflows Runbook

Durable workflows are false-by-default at launch. An app runs workflows only when
both gates are true:

- `zeroship.apps.workflows_enabled = true`
- `zeroship.plans.workflows_allowed = true` for the app's current plan

The two operator kill-switches live in `workflow_manager.workflow_rollout_config`
(`id = 'global'`), in the workflow service's own schema. The service reads that
row and cannot write it, so every statement below runs under an administrative
credential.

There is no default row and no default for a missing one. A deployment that has
never published the `global` row fails every policy observation, which refuses
every app rather than admitting one: provision it before enabling any app.

## Enable An App

```sql
BEGIN;
UPDATE zeroship.plans
   SET workflows_allowed = true, updated_at = now()
 WHERE id = (SELECT plan_id FROM zeroship.apps WHERE id = :app_id);
UPDATE zeroship.apps
   SET workflows_enabled = true, updated_at = now()
 WHERE id = :app_id;
COMMIT;
```

Verify:

```sql
SELECT a.id, a.workflows_enabled, p.id AS plan_id, p.workflows_allowed
  FROM zeroship.apps a
  JOIN zeroship.plans p ON p.id = a.plan_id
 WHERE a.id = :app_id;
```

## Disable An App

```sql
UPDATE zeroship.apps
   SET workflows_enabled = false, updated_at = now()
 WHERE id = :app_id;
```

Effects: new `start()` calls are refused, public signal ingress for the app is
refused, schedule fan-out is skipped, and scheduler dispatch no longer advances
queued, sleeping, waiting, or compensating runs for that app. Existing journal
rows are left in place.

## Where the scheduler runs

The workflow manager (`zeroship-workflow-server`, over
`crates/zeroship-workflow-manager/`) owns calendar evaluation, the durable job
queue and the zone claim workers pull from, delivery attempts, recovery
responsibility, and each execution zone's capacity target. It reads the
platform database under its own login and holds no creator database connection.
Outside its own `workflow_manager` schema that login is column-scoped: named
columns of `zeroship.worker_instances` for worker identity, zone and lease, and
the shared assertion replay store
(`db/migrations-ts/20260911000050_workflow_platform_grants.ts` and
`db/migrations-ts/20260914000600_app_execution_zones.ts`, whose union is the
whole of it); it holds nothing on `zeroship.apps`. Control-owned app and plan
facts sit outside it. They arrive over
Control's authenticated `POST /v1/app-facts` endpoint, behind the
`AppFactsSource` capability in
`crates/zeroship-workflow-manager/src/app_facts.rs`, so the manager holds no
binding on Control's tables.

It holds no creator artifact store either. The maintenance operations that
record a journal `deploys` row need a deployment's workflow declarations, and
they take them the same way: Control parsed the bundle when it published, and
asserts that summary over `POST /v1/deploy-registration`
(`crates/zeroship-control/src/deployment_hold_api.rs`), behind the
`DeployRegistrationSource` capability in
`crates/zeroship-workflow/src/deploy_registrations.rs`. A host that does hold the
bundle derives the same summary from the bytes instead. Reading the artifacts is
refused by name on this process, not silently skipped.

A creator run is executed by any enrolled `zeroship-worker` in the app's
execution zone (`crates/zeroship-worker/src/workflow_host.rs`). A worker asks
the manager for as many claimable `advance` jobs as it has free execution
slots, and the claim pages only the apps of the zone frozen on the worker's
instance row (`Coordinator::claim_in_zone` in
`crates/zeroship-workflow-manager/src/coordinator/jobs.rs`); there is no
registration or assignment step, and no worker reaches an app of another zone.
The worker prepares a claimed app when its first job arrives and keeps it in a
bounded cache. That host holds NO journal: it reaches the workflow service for
every journal fact, and writes payload bytes into the worker's own object store
under the reserved `platform:workflow` namespace, which creator code cannot
address. It receives work only as a job the manager delivered; it runs no
due-work scan and no maintenance loop of its own. The service's own maintenance
lane claims every other job kind.

The execution zone is the isolation unit. A worker can read the environment,
data key and metadata of every app in its zone, and run calls and claims are
admitted for exactly those apps, so apps that must not share a compromised
worker belong in separate execution zones.

Control publishes deployment lifecycle intents to the manager and arbitrates
deployment holds. Each register, activate and disable message names the app's
frozen execution zone, which the queue scope the manager creates for the app
records. Control reaches no creator journal, and there is no advance transport
through the gateway.

## Zone backlog and capacity

Each execution zone has one row in `workflow_manager.capacity_targets`, whose
`id` is the zone. The driver's capacity lane (`Capacity::reconcile` in
`crates/zeroship-workflow-manager/src/capacity.rs`) visits it on every pass,
reading each app's policy through the service's own policy source: an app's
demand is its live leases plus as many claimable rows as its `max_running`
leaves room for. `desired` is the zone's demand held between
`workflow.capacity_min_slots` and `workflow.capacity_max_slots`; a rise applies
at once and a fall only after `workflow.capacity_hold_down_ms`. A deployment
whose workers are started outside the platform is a static pool of
`workflow.static_pool_slots` execution slots, and a target above it is recorded
`state = 'refused'` with `refusal = 'pool_exhausted'`.

```sql
SELECT id AS zone, desired, state, refusal,
       backlog_depth, oldest_available_at,
       exhausted_jobs, backed_off_jobs, withheld_jobs
  FROM workflow_manager.capacity_targets;
```

`backlog_depth` and `oldest_available_at` describe the claimable demand the
last complete visit found. The other three count creator work that cannot be
delivered now and never counts toward demand:

- `exhausted_jobs`: rows whose counted executions reached the app's
  `max_delivery_attempts`. They stay unsettled and are never claimed again
  unless policy raises the budget. Resolve the run through management (cancel or
  restart) or a redeploy.
- `backed_off_jobs`: rows given back to the queue, by a journal deferral or a
  worker that could not prepare the app, and still inside their back-off. A
  preparation failure's back-off grows with each consecutive one, so a count
  that stays high names an app no worker can prepare; check its environment and
  deployment.
- `withheld_jobs`: unsettled rows of apps whose policy withholds dispatch or
  which Control deleted. When the closing lane abandons a deleted app it settles
  that app's unsettled creator rows `Rejected`.

A worker stopping for scale-down drains for up to `worker.shutdown_timeout`,
which the worker refuses at boot below what one delivery needs: its app's
preparation, its execution at the ceiling and its settlement
(`validate_shutdown_timeout` in `crates/zeroship-worker/src/workflow_host.rs`).
The orchestrator's termination grace must cover that drain and the instance's
retirement call, as `stop_grace_period` does in
`deploy/compose/docker-compose.yml`. Work cut off by a shorter grace runs again
on another worker once its lease lapses.


## Dispatch Pause

The workflow manager's policy provider requires a provisioned global
`source_validity_ms` and complete `plans.workflow_policy_json` values. Choose the
finite validity as an operator bound on stale authority, then use native
`ControlPolicyStore::set_rollout` for the switches and
`PlanPolicyStore::set_plan_policy` for the plan rows, or explicit SQL
provisioning. Both writes are operator acts on separate credentials, and no
service login covers either: `zeroship_workflow` holds only SELECT on
`workflow_rollout_config`
(`db/migrations-ts/20260911000060_workflow_policy_tables.ts`) and nothing at all
on `zeroship.plans`
(`db/migrations-ts/20260911000050_workflow_platform_grants.ts`).
Plan policy follows the closed `AppPolicy` contract in
`crates/zeroship-core/src/workflow_policy.rs`. Missing fields are not defaulted.
The SQL placeholders below require that chosen validity when inserting the global
row; updates preserve its current bound. The service's policy cache keeps each
observation until its original deadline, and workers hold no policy of their
own, so a switch update does not prove execution quiescence.

Use this when the replay engine is suspect.

```sql
INSERT INTO workflow_manager.workflow_rollout_config
       (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by)
VALUES ('global', true, false, :source_validity_ms, :operator)
ON CONFLICT (id) DO UPDATE SET
  dispatch_paused = true,
  updated_at = now(),
  updated_by = EXCLUDED.updated_by;
```

Resume:

```sql
UPDATE workflow_manager.workflow_rollout_config
   SET dispatch_paused = false, updated_at = now(), updated_by = :operator
 WHERE id = 'global';
```

Effect: the switch masks `dispatch` in every app's observed policy. A worker's
zone claim passes each app whose observation has dispatch off, and a job the
journal is handed while dispatch is off is deferred and given back to the queue
until that observation lapses. A heartbeat for an execution already running
extends nothing, which interrupts it at its next renewal; its row returns to the
queue. Queued, sleeping and waiting work stays durable in the journal. Cached
observations answer until they lapse, so both the pause and the resume take
effect within one `source_validity_ms` of the update.

## Ingress Disable

Use this when the public signal edge is suspect.

```sql
INSERT INTO workflow_manager.workflow_rollout_config
       (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by)
VALUES ('global', false, true, :source_validity_ms, :operator)
ON CONFLICT (id) DO UPDATE SET
  ingress_disabled = true,
  updated_at = now(),
  updated_by = EXCLUDED.updated_by;
```

Re-enable:

```sql
UPDATE workflow_manager.workflow_rollout_config
   SET ingress_disabled = false, updated_at = now(), updated_by = :operator
 WHERE id = 'global';
```

Effect: the manager refuses to grant an ingress capability and the worker host
refuses ingestion for every app, before token verification or any journal write.
App-credentialed `run.signal` remains available.

## Drain

Pause new claims first:

```sql
UPDATE workflow_manager.workflow_rollout_config
   SET dispatch_paused = true, updated_at = now(), updated_by = :operator
 WHERE id = 'global';
```

When the manager reports no delivery in flight for the app, no execution is
running. Queued, sleeping, waiting and compensating work stays durable in the
service's journal and resumes after the switch is cleared.

## Inspecting a run, restarting one, and reading GC state

Run state is IN the platform database. Each app's runs, steps, signals,
subscriptions and staged payload references live in the `workflow_manager`
schema, reached only by the workflow service; the durable job queue, delivery
attempts and deadlines live in that same schema. No creator database holds
workflow state and no worker opens one - a host runs creator code and asks the
service for every journal fact. Not an operator SQL surface.

- Run state, outputs and restart go through the creator-authorized handle:
  `env.workflows` in app code, and the manager's management commands
  (`crates/zeroship-workflow-manager/`) for operator-initiated control. Do not
  rewrite journal rows by hand: restart updates payload references, the signal
  epoch, audit fields and wake state together.
- Payload and deployment retention run as delivered jobs
  (`crates/zeroship-workflow/src/service/deployment_retention.rs` and the
  manager's retention lane), fenced by the deployment holds Control arbitrates.

## Dashboards And Alerts

- Delivery retry rate: count job delivery attempts that follow an expired
  execution lease. Alert on sustained elevation.
- Stalled rate: count terminal `state = 'stalled'` runs per window. Alert on
  any unexplained stalled run. A run reaches it when the app policy's
  `maxStuckDispatches` reclaimed dispatches of one frontier report nothing, so
  a rise here reads as bodies that hang or workers that die mid-dispatch, and
  the run's recorded `StalledError` carries the count that tripped. A run whose
  *rollback* stops reporting trips the same budget but rests `failed`, so it is
  invisible here and shows up under abandoned rollback below.
- Queue lag: track the oldest due job the manager has not delivered, and the
  oldest pending fanout page. To add: per-lane lag gauges emitted after each
  drain.
- Journal and payload growth: per-app journal size and staged payload bytes.
  Alert on growth rate outside the measured launch envelope.
- Ingress reject rate: count creator signal ingress 4xx/5xx outcomes by reason.
- Incomplete rollback rate: count terminal runs whose recorded error carries a
  `compensation.outcome` other than `completed`. The outcome is a field of the
  encoded `error` on the run's generation row, not a column, so this reads the
  JSON rather than filtering on one. Alert immediately on either value.
  `partial` means a compensator reported a failure, and `compensation.failures`
  names it. `abandoned` means a compensator never reported at all, so the
  platform stopped waiting and the undo may have half-applied;
  `compensation.abandoned` names the steps nobody discharged and
  `compensation.reason` carries the liveness verdict that stopped it. Both need
  operator review, and `abandoned` needs it first.
