import { grant, raw, t, table } from "@zeroship/migrate";

export default {
  name: "project_data_keys",
  schema() {
    table("project_data_keys", { schema: "zeroship" }).create({
      columns: { id: t.bigInt().notNull().identity(), project_id: t.text().notNull(), ciphertext: t.bytes().notNull() },
      primaryKey: ["id"],
    });
    table("project_data_keys", { schema: "zeroship" }).unique("project_data_keys_natural_key").add({ columns: ["project_id"] });
    raw({
      sql: 'ALTER TABLE "zeroship"."project_data_keys" ALTER COLUMN "project_id" TYPE text COLLATE "C"',
      reason: "match the referenced project identity collation",
    });
    table("project_data_keys", { schema: "zeroship" }).foreignKey("project_data_keys_project_fkey").add({
      columns: ["project_id"],
      references: { table: "projects", columns: ["id"], schema: "zeroship" },
      onDelete: "cascade",
      onUpdate: "restrict",
    });
    grant({
      privileges: ["select", "insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["project_data_keys"] },
      to: ["zeroship_control"],
    });
  },
};
