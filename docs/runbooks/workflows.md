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
queue, delivery attempts, placement and recovery responsibility. It reads the
platform database under its own login and holds no creator database connection.

A creator run is executed by the `zeroship-worker` that the manager placed the
app on (`crates/zeroship-worker/src/workflow_host.rs`). That host owns the
creator journal in the app database and its payloads in the app object store.
It receives work only as a job the manager delivered; it runs no due-work scan
and no maintenance loop of its own.

Control publishes deployment lifecycle intents to the manager and arbitrates
deployment holds. It reaches no creator journal, and there is no advance
transport through the gateway.


## Dispatch Pause

The workflow manager's policy provider requires a provisioned global
`source_validity_ms` and complete `plans.workflow_policy_json` values. Choose the
finite validity as an operator bound on stale authority, then use native
`ControlPolicyStore::set_rollout` and `set_plan_policy`, or explicit SQL
provisioning. Either way the credential has to reach both the switches in
`workflow_manager` and the plan rows in `zeroship`, which no service login does.
Plan policy follows the closed `AppPolicy` contract in
`crates/zeroship-core/src/workflow_policy.rs`. Missing fields are not defaulted.
The SQL placeholders below require that chosen validity when inserting the global
row; updates preserve its current bound. Manager and worker caches retain their
original lease deadlines, so a switch update does not prove execution quiescence.

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

Effect: scheduler dispatch ticks return zero before cancel reaping,
waiting-run rearm, or due-run claiming. In-flight dispatches finish their
commit; everything else remains durably parked in the journal.

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
creator journal and resumes after the switch is cleared.

## Inspecting a run, restarting one, and reading GC state

Run state is not in the platform database. Each app's runs, steps, signals,
subscriptions and staged payload references live in that app's own creator
schema, reached only by the worker hosting it; the durable job queue, delivery
attempts and deadlines live in the manager's `workflow_manager` schema in the
platform database. That schema also carries journal tables under a
`__zeroship_workflow_` prefix which nothing writes and nothing reads: an empty
one there says nothing about an app. Neither is an operator SQL surface.

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
