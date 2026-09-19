import { table, t } from "@zeroship/migrate";

// Assignment generators are supplied by the confined charter.
export default {
  name: "create_todos",
  schema() {
    table("users").create({
      columns: {
        email: t.text().required().unique(),
        name: t.text().required(),
        handle: t.text().required().unique(),
      },
    });
    table("todos").create({
      columns: {
        userId: t.text().required().references("users", "id", { relation: "user" }),
        title: t.text().required(),
        priority: t.text().required().default("medium"),
        tags: t.json(),
        done: t.boolean().required().default(false),
        archived: t.boolean().required().default(false),
      },
      indexes: [{ name: "todos_user_idx", on: ["userId"] }],
    });
  },
};
