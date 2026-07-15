import { table, t } from "@zeroship/migrate";

export default {
  name: "create_hits",
  up() {
    table("hits").create({
      columns: {
        path: t.text().notNull(),
      },
    });
  },
};
