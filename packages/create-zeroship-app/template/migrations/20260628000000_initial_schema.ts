import { table, t } from "@zeroship/migrate";

export default {
  name: "initial_schema",

  schema() {
    table("users").create({
      columns: {
        email: t.text().required().unique(),
        name: t.text().required(),
      },
    });

    table("notes").create({
      columns: {
        title: t.text().required(),
        body: t.text(),
      },
    });
  },
};
