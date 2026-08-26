import { table, t } from "zero-migrate";

export const name = "create_dml_flow_items";

export default {
  schema() {
    table("dml_flow_items").create({
      columns: {
        id: t.int().primaryKey(),
        label: t.text().notNull(),
        stage: t.text().notNull(),
        score: t.int().notNull(),
        payload: t.bytes().notNull(),
      },
    });
  },
};
