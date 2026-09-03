import { table, t } from "@zeroship/migrate";

export const name = "create_widgets";
export default {
  schema() {
    table("widgets").create({
      columns: {
        label: t.text().notNull(),
        status: t.string({ length: 32 }).notNull().default("new"),
      },
    });
    table("widgets").column("qty").add({ type: t.int() });
  },
};
