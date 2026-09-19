import { table, t } from "@zeroship/migrate";

export const name = "create_dml_flow_items";

export default {
  schema() {
    table("dml_flow_items").create({
      columns: {
        id: t.int().primaryKey(),
        label: t.text().required(),
        stage: t.text().required(),
        score: t.int().required(),
        payload: t.bytes().required(),
      },
    });
  },
};
