import { table, t } from "@zeroship/migrate";

export default {
  name: "initial_schema",

  up() {
    table("users").create({
      columns: {
        email: t.text().notNull().unique(),
        name: t.text().notNull(),
      },
    });

    table("notes").create({
      columns: {
        title: t.text().notNull(),
        body: t.text(),
      },
    });
  },
};
