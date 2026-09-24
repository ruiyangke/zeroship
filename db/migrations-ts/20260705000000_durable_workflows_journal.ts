import { table, grant } from "@zeroship/migrate";
import { deploymentSchema } from "../../crates/zeroship-workflow-manager/schema/deployments/schema.ts";

const schema = "zeroship";
const controlTables = ["app_deploys", "app_deploy_holds"];

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

    zs("app_deploys").foreignKey("app_deploys_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });

    grant({ privileges: ["select", "insert", "update", "delete"], on: controlTableTarget(), to: ["zeroship_control"] });
  },
};
