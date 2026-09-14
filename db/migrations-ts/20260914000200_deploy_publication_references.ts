import { table } from "@zeroship/migrate";

// A receipt and an intent each name the deployment they selected by
// `(app_id, deploy_id)`. The composite references follow the table creation in
// 20260914000100_deploy_publication.ts because a composite key must match the
// referenced `app_deploys` collation at every position when it is lowered
// against the live catalog, and that migration is where these columns take the
// bytewise collation. It also declares the full `(app_id, deploy_id)` index on
// both tables, so lowering these references never synthesizes a supporting
// index whose presence would then change how the same migration lowers
// against an already migrated catalog.
//
// NO ACTION rather than RESTRICT: the app row's own cascades can then remove
// the deployment and its receipts and intents in one statement. Retention never
// deletes a deployment row; it marks it reclaimed.
export default {
  name: "deploy_publication_references",
  schema() {
    table("app_deploy_commands", { schema: "zeroship" })
      .foreignKey("app_deploy_commands_deployment_fkey")
      .add({
        columns: ["app_id", "deploy_id"],
        references: { table: "app_deploys", columns: ["app_id", "id"] },
        onDelete: "noAction",
      });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .foreignKey("app_lifecycle_intents_deployment_fkey")
      .add({
        columns: ["app_id", "deploy_id"],
        references: { table: "app_deploys", columns: ["app_id", "id"] },
        onDelete: "noAction",
      });
  },
};
