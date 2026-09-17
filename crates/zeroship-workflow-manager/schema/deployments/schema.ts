import { table, t, now } from "../../../../packages/zero-migrate/dist/index.js";

// Platform deployment metadata. The customer workflow journal does not use it.
export function deploymentSchema(namespace) {
  const deploys = table("app_deploys", { schema: namespace });
  deploys.create({
    columns: {
      id: t.text().required(),
      app_id: t.text().required(),
      deploy_hash: t.text().required(),
      manifest_json: t.text().required(),
      created_at: t.timestamp().required().default(now()),
      activated_at: t.timestamp(),
      retention_state: t.text().required().default("available"),
      retention_lock: t.bigInt().required().default(0),
    },
    primaryKey: ["id"],
  });
  deploys.index("app_deploys_app_id_deploy_hash_key").add({ on: ["app_id", "deploy_hash"], unique: true });
  deploys.index("app_deploys_app_id_id_key").add({ on: ["app_id", "id"], unique: true });
  deploys.index("app_deploys_app_created_idx").add({ on: ["app_id", { column: "created_at", order: "desc" }] });

  table("app_deploy_holds", { schema: namespace }).create({
    columns: {
      id: t.text().required(),
      app_id: t.text().required(),
      deploy_id: t.text().required(),
      holder_id: t.text().required(),
      generation: t.bigInt().required(),
      state: t.text().required(),
    },
    primaryKey: ["id"],
    foreignKeys: [{
      name: "app_deploy_holds_deployment_fkey",
      columns: ["app_id", "deploy_id"],
      references: { schema: namespace, table: "app_deploys", columns: ["app_id", "id"] },
      onDelete: "restrict",
    }],
  });
  table("app_deploy_holds", { schema: namespace })
    .index("app_deploy_holds_scope_key")
    .add({ on: ["app_id", "deploy_id", "holder_id"], unique: true });
}
