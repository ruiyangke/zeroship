import { table, t, now, grant } from "@zeroship/migrate";
import { deploymentSchema } from "../../crates/zeroship-workflow-manager/schema/deployments/schema.ts";

const schema = "zeroship";
const controlTables = [
  "app_deploys",
  "app_deploy_holds",
  "workflow_rollout_config",
  "workflow_policy_ledger",
];

function zs(name) {
  return table(name, { schema });
}

function controlTableTarget() {
  return { kind: "table", schema, names: controlTables };
}

export default {
  name: "durable_workflows_journal",
  schema() {
    deploymentSchema(schema);

    zs("workflow_rollout_config").create({
      columns: {
        id: t.text().required().default("global"),
        dispatch_paused: t.boolean().required().default(false),
        ingress_disabled: t.boolean().required().default(false),
        source_validity_ms: t.bigInt().required(),
        updated_at: t.timestamp().required().default(now()),
        updated_by: t.text(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_rollout_config").check("workflow_rollout_config_id_check").add({ expr: (c) => c("id").eq("global") });
    zs("workflow_rollout_config").check("workflow_rollout_config_validity_check").add({ expr: (c) => c("source_validity_ms").gt(0) });

    zs("workflow_policy_ledger").create({
      columns: {
        id: t.text().required(),
        revision: t.bigInt().required().default(0),
        policy_json: t.json(),
        source_validity_ms: t.bigInt(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_policy_ledger").check("workflow_policy_ledger_publication_check").add({
      expr: (c) => c("revision").eq(0).and(c("policy_json").isNull(), c("source_validity_ms").isNull())
        .or(c("revision").gt(0).and(c("policy_json").isNotNull(), c("source_validity_ms").isNotNull(), c("source_validity_ms").gt(0))),
    });

    zs("app_deploys").foreignKey("app_deploys_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });

    grant({ privileges: ["select", "insert", "update", "delete"], on: controlTableTarget(), to: ["zeroship_control"] });
  },
};
