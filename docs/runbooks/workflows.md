# Durable Workflows Runbook

Durable workflows are false-by-default at launch. An app runs workflows only when
both gates are true:

- `zeroship.apps.workflows_enabled = true`
- `zeroship.plans.workflows_allowed = true` for the app's current plan

The two operator kill-switches live in `zeroship.workflow_rollout_config`
(`id = 'global'`). If the row is missing, both switches are treated as off.

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
refused, schedule fan-out is skipped, and the dispatch claim sweep no longer
advances queued, sleeping, waiting, or compensating runs for that app. Existing
journal rows are left in place.

## Dispatch Pause

Use this when the replay engine is suspect.

```sql
INSERT INTO zeroship.workflow_rollout_config
       (id, dispatch_paused, ingress_disabled, updated_by)
VALUES ('global', true, false, :operator)
ON CONFLICT (id) DO UPDATE SET
  dispatch_paused = true,
  updated_at = now(),
  updated_by = EXCLUDED.updated_by;
```

Resume:

```sql
UPDATE zeroship.workflow_rollout_config
   SET dispatch_paused = false, updated_at = now(), updated_by = :operator
 WHERE id = 'global';
```

Effect: `workflow_engine` ticks return zero before cancel reaping, waiting-run
rearm, or due-run claiming. In-flight dispatches finish their commit; everything
else remains durably parked in the journal.

## Ingress Disable

Use this when the public signal edge is suspect.

```sql
INSERT INTO zeroship.workflow_rollout_config
       (id, dispatch_paused, ingress_disabled, updated_by)
VALUES ('global', false, true, :operator)
ON CONFLICT (id) DO UPDATE SET
  ingress_disabled = true,
  updated_at = now(),
  updated_by = EXCLUDED.updated_by;
```

Re-enable:

```sql
UPDATE zeroship.workflow_rollout_config
   SET ingress_disabled = false, updated_at = now(), updated_by = :operator
 WHERE id = 'global';
```

Effect: the gateway forwards the public signal route to control and returns the
control 503 response. The control terminus rejects before token verification or
journal writes. App-credentialed `run.signal` remains available.

## Drain

Pause new claims first:

```sql
UPDATE zeroship.workflow_rollout_config
   SET dispatch_paused = true, updated_at = now(), updated_by = :operator
 WHERE id = 'global';
```

Wait for live leases to clear:

```sql
SELECT state, count(*) AS runs
  FROM zeroship.workflow_runs
 WHERE claimed_by IS NOT NULL
   AND lease_expires > now()
 GROUP BY state
 ORDER BY state;
```

When this returns no rows, no dispatch is in flight. Queued, sleeping, waiting,
and compensating rows remain durable and resume after the switch is cleared.

## Restart A Run

Preferred path is the control API restart operation with operator credentials.
Use full restart unless support has identified a safe step boundary.

Inspect restart state:

```sql
SELECT id, state, workflow_name, restart_count, restarted_at,
       restarted_from_ordinal, restarted_by
  FROM zeroship.workflow_runs
 WHERE id = :run_id;
```

If using direct SQL during incident response, do not rewrite journal rows by
hand. Use the control restart path so blob refcounts, signal epoch, audit fields,
and wake state are updated together.

## Read A Journal

Run summary:

```sql
SELECT id, app_id, workflow_name, state, wake_at, claimed_by, lease_expires,
       next_ordinal, stuck_strikes, compensation_target,
       compensation_outcome, output_kind, error
  FROM zeroship.workflow_runs
 WHERE id = :run_id;
```

Step prefix:

```sql
SELECT ordinal, name, name_occurrence, kind, state, output_kind,
       signal_type, consumed_signal_id, child_run_id,
       compensation_state, compensation_attempts, error
  FROM zeroship.workflow_steps
 WHERE run_id = :run_id
 ORDER BY ordinal, name_occurrence;
```

Signals:

```sql
SELECT id, type, origin, delivery, topic, consumed_by, created_at
  FROM zeroship.workflow_signals
 WHERE run_id = :run_id
 ORDER BY created_at, id;
```

## GC Status

Workflow blob table:

```sql
SELECT count(*) AS blobs,
       sum(size) AS bytes,
       sum(CASE WHEN refcount = 0 THEN 1 ELSE 0 END) AS unreferenced
  FROM zeroship.workflow_blobs;
```

Terminal-run retention backlog:

```sql
SELECT state, count(*) AS runs, min(terminal_at) AS oldest_terminal
  FROM zeroship.workflow_runs
 WHERE state IN ('completed', 'failed', 'cancelled', 'stalled')
   AND terminal_at IS NOT NULL
 GROUP BY state
 ORDER BY oldest_terminal;
```

The retention sweep prunes only terminal runs whose `terminal_at` is older than
the operator-tunable `CONTROL_WORKFLOW_RETENTION_WINDOW_MS`; the code default is
`DEFAULT_RETENTION_WINDOW_MS` (7 days). `compensating` is not terminal and is
never eligible. Each tick logs counters for reaped runs, steps, signals,
subscriptions, broadcasts, and blobs.

Largest apps by retained workflow bytes:

```sql
SELECT r.app_id, sum(r.journal_bytes) AS journal_bytes,
       sum(r.blob_bytes) AS blob_bytes
  FROM zeroship.workflow_runs r
 GROUP BY r.app_id
 ORDER BY sum(r.journal_bytes + r.blob_bytes) DESC
 LIMIT 20;
```

Expired broadcast/subscription backlog:

```sql
SELECT
  (SELECT count(*) FROM zeroship.workflow_broadcasts
    WHERE expires_at < now()) AS expired_broadcasts,
  (SELECT count(*) FROM zeroship.workflow_subscriptions
    WHERE expires_at IS NOT NULL AND expires_at < now()) AS expired_subscriptions;
```

## Dashboards And Alerts

- Lease-reclaim rate: count dispatches that claim rows with expired leases. To
  add: counter in `workflow_engine::claim_due_batch` when `lease_expires <= now()`
  on the selected row. Alert on sustained elevation.
- `stuck_strikes` / stalled rate: query rows with `stuck_strikes > 0` and count
  terminal `state = 'stalled'` per window. Alert on any unexplained stalled run.
- Sweep lag: track oldest due item for claim, schedule, fan-out, and GC sweeps.
  Claim lag is `min(wake_at)` for eligible runs; schedule lag is
  `min(next_fire_at)` for eligible schedules; fan-out lag is oldest pending
  broadcast; GC lag is oldest unreferenced blob past grace. To add: per-sweep
  lag gauges emitted after each tick.
- Journal and blob growth: sum `workflow_runs.journal_bytes`,
  `workflow_runs.blob_bytes`, and `workflow_blobs.size` by app and total.
  Alert on growth rate outside the measured launch envelope.
- Ingress reject rate: count public ingress 4xx/5xx outcomes by reason. To add:
  counters in the gateway public signal handler and control ingress terminus.
- Partial compensation rate: count terminal runs with
  `compensation_outcome = 'partial'`. Alert immediately; this means at least one
  compensator failed and operator review is required.
