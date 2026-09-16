import { table, t, now } from "../../../../packages/zero-migrate/dist/index.js";

// Platform deployment metadata. The customer workflow journal does not use it.
export function deploymentSchema(namespace) {
  const deploys = table("app_deploys", { schema: namespace });
  deploys.create({
    columns: {
      id: t.text().notNull(),
      app_id: t.text().notNull(),
      deploy_hash: t.text().notNull(),
      manifest_json: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
      activated_at: t.timestamp(),
      retention_state: t.text().notNull().default("available"),
      retention_lock: t.bigInt().notNull().default(0),
    },
    primaryKey: ["id"],
  });
  deploys.index("app_deploys_app_id_deploy_hash_key").add({ on: ["app_id", "deploy_hash"], unique: true });
  deploys.index("app_deploys_app_id_id_key").add({ on: ["app_id", "id"], unique: true });
  deploys.index("app_deploys_app_created_idx").add({ on: ["app_id", { column: "created_at", order: "desc" }] });

  table("app_deploy_holds", { schema: namespace }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.text().notNull(),
      deploy_id: t.text().notNull(),
      holder_id: t.text().notNull(),
      generation: t.bigInt().notNull(),
      state: t.text().notNull(),
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
