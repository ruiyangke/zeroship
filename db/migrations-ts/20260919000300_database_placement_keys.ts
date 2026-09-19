import { table } from "@zeroship/migrate";

// The composite keys that carry the decoupling's two predicates: a database is
// placed on a cluster in its project's zone, and a binding joins an app and a
// database of one project. From
// docs/proposals/2026-08-28-app-database-decoupling.md.
//
// A SEPARATE FILE FROM THE TABLES THEY CONSTRAIN, for the reason
// 20260906000200_apps_project_ownership_key.ts states: the engine lowers a
// foreign key against a catalog snapshot taken before the migration runs, and
// the bytewise collation these identity columns need is applied by a `raw`
// island that no snapshot and no authored contract can see. A composite key is
// lowered position by position and refuses a pair whose collations differ -
// correctly, since that pair is the silent index degradation the collation rule
// exists to stop. Authored one migration later, every column on both sides is
// live and every one of them is `C`.
export default {
  name: "database_placement_keys",
  schema() {
    // And the app's zone copy cannot disagree with its project's.
    table("apps", { schema: "zeroship" })
      .foreignKey("apps_project_zone_fkey")
      .add({
        columns: ["project_id", "execution_zone_id"],
        references: { table: "projects", columns: ["id", "execution_zone_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    // The zone is its PROJECT'S zone ...
    table("databases", { schema: "zeroship" })
      .foreignKey("databases_project_zone_fkey")
      .add({
        columns: ["project_id", "execution_zone_id"],
        references: { table: "projects", columns: ["id", "execution_zone_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    // ... and its CLUSTER'S zone. Together: a database is placed on a cluster in
    // its project's zone, structurally, with no trigger and nothing to forget.
    table("databases", { schema: "zeroship" })
      .foreignKey("databases_placement_fkey")
      .add({
        columns: ["datastore_id", "execution_zone_id"],
        references: { table: "datastores", columns: ["id", "execution_zone_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    // THE TWO EDGES. Both agree on project_id, so an app can bind only a
    // database in its own project, on every write to either side.
    table("database_bindings", { schema: "zeroship" })
      .foreignKey("database_bindings_app_project_fkey")
      .add({
        columns: ["app_id", "project_id"],
        references: { table: "apps", columns: ["id", "project_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("database_bindings", { schema: "zeroship" })
      .foreignKey("database_bindings_database_project_fkey")
      .add({
        columns: ["database_id", "project_id"],
        references: { table: "databases", columns: ["id", "project_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
  },
};
