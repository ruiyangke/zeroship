import { createFunction, grant, raw, table } from "../../../packages/zero-migrate/dist/index.js";

// Policy readers hold shared locks through their workflow commit. Triggers
// fence every writer, including an INSERT into a previously absent billing row.
// These functions run with the invoker's privileges and only coordinate locks.
export function workflowPlatformPolicy() {
  createFunction({
    schema: "zeroship", name: "workflow_policy_lock", language: "procedural", returns: "void",
    args: [
      { name: "resource_kind", type: "text" },
      { name: "resource_id", type: "text" },
      { name: "exclusive_lock", type: "boolean" },
    ],
    body: `DECLARE lock_key bigint;
BEGIN
  IF resource_kind NOT IN ('app', 'plan', 'organization', 'spend', 'rollout')
     OR resource_kind IS NULL OR resource_id IS NULL OR resource_id = ''
     OR exclusive_lock IS NULL THEN
    RAISE EXCEPTION 'invalid workflow policy lock resource';
  END IF;
  lock_key := pg_catalog.hashtextextended('workflow-policy:' || resource_kind || ':' || resource_id, 0);
  IF exclusive_lock THEN
    PERFORM pg_catalog.pg_advisory_xact_lock(lock_key);
  ELSE
    PERFORM pg_catalog.pg_advisory_xact_lock_shared(lock_key);
  END IF;
END;`,
  });
  raw({
    sql: "REVOKE ALL ON FUNCTION zeroship.workflow_policy_lock(text,text,boolean) FROM PUBLIC",
    reason: "the function grant identifies its overload explicitly",
  });
  raw({
    sql: "GRANT EXECUTE ON FUNCTION zeroship.workflow_policy_lock(text,text,boolean) TO zeroship_control, zeroship_workflow",
    reason: "policy writers and the workflow reader coordinate on the same lock function",
  });
  for (const [name, column, kind] of [
    ["apps", "id", "app"],
    ["plans", "id", "plan"],
    ["organization_billing_status", "organization_id", "organization"],
    ["app_spend_state", "app_id", "spend"],
    ["workflow_rollout_config", "id", "rollout"],
  ]) {
    const fn = `workflow_policy_fence_${kind}`;
    createFunction({
      schema: "zeroship", name: fn, language: "procedural", returns: "trigger",
      body: `BEGIN
  IF TG_OP = 'UPDATE' AND NEW.${column} IS DISTINCT FROM OLD.${column} THEN
    RAISE EXCEPTION 'workflow policy resource identity is immutable';
  END IF;
  IF TG_OP = 'DELETE' THEN
    PERFORM zeroship.workflow_policy_lock('${kind}', OLD.${column}::text, true);
    RETURN OLD;
  END IF;
  PERFORM zeroship.workflow_policy_lock('${kind}', NEW.${column}::text, true);
  RETURN NEW;
END;`,
    });
    raw({
      sql: `REVOKE ALL ON FUNCTION zeroship.${fn}() FROM PUBLIC`,
      reason: "trigger entrypoints are not callable service capabilities",
    });
    table(name, { schema: "zeroship" }).trigger(fn).create({
      timing: "before", events: ["insert", "update", "delete"], forEach: "row", execute: fn,
    });
  }
  raw({
    sql: "INSERT INTO zeroship.workflow_rollout_config (id,dispatch_paused,ingress_disabled) VALUES ('global',false,false) ON CONFLICT (id) DO NOTHING",
    reason: "workflow startup requires an explicit operator policy row",
  });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["zeroship_workflow"] });
  // Column grants intentionally omit UPDATE. A workflow process can read
  // current policy but cannot change the platform's admission authority.
  for (const [name, columns] of [
    ["apps", "id,plan_id,organization_id,workflows_enabled,archived_at,deleted_at"],
    ["plans", "id,workflows_allowed,archived,runtime_limits_json"],
    ["organization_billing_status", "organization_id,state"],
    ["app_spend_state", "app_id,state"],
    ["workflow_rollout_config", "id,dispatch_paused,ingress_disabled"],
    ["worker_instances", "id,status,public_key"],
  ]) {
    raw({
      sql: `GRANT SELECT (${columns}) ON zeroship.${name} TO zeroship_workflow`,
      reason: "workflow reads only the platform policy and worker identity columns it needs",
    });
  }
}
