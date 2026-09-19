import { table, t } from "@zeroship/migrate";

export default {
  name: "create_hits",
  schema() {
    table("hits").create({
      columns: {
        path: t.text().required(),
      },
    });
  },
};
