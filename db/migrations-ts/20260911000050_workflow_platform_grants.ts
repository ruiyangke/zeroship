import { grant, raw } from "@zeroship/migrate";

// The reach `zeroship_workflow` holds outside its own schema: USAGE on the two
// platform schemas it reads across, column-scoped SELECT on the Control tables
// its coordinator and policy reads project, and DML on the shared assertion
// replay store it settles service assertions against. The role is created in
// 20260911000000_workflow_coordination.ts, so this file sorts after it.
export default {
  name: "workflow_platform_grants",
  schema() {
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship", "service_authn"] }, to: ["zeroship_workflow"] });
    raw({
      sql: "GRANT SELECT (id,status,public_key) ON zeroship.worker_instances TO zeroship_workflow",
      reason: "the coordinator verifies enrolled worker identity without customer-data access",
    });
    raw({
      sql: "GRANT SELECT (id,plan_id,workflows_enabled,archived_at) ON zeroship.apps TO zeroship_workflow; GRANT SELECT (id,workflows_allowed,archived,workflow_policy_json) ON zeroship.plans TO zeroship_workflow",
      reason: "workflow policy observations read only Control-owned contributors",
    });
    raw({
      sql: "GRANT SELECT (deploy_hash) ON zeroship.apps TO zeroship_workflow; GRANT SELECT (id,app_id,deploy_hash,retention_state) ON zeroship.app_deploys TO zeroship_workflow",
      reason: "latest restart observes the current Control deployment without creator data or catalog write authority",
    });
    grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["workflow_rollout_config"] }, to: ["zeroship_workflow"] });
    grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["workflow_policy_ledger"] }, to: ["zeroship_workflow"] });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "service_authn", names: ["service_assertion_replay"] },
      to: ["zeroship_workflow"],
    });
  },
};
