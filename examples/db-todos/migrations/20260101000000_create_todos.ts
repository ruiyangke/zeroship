import { table, t } from "@zeroship/migrate";

// Assignment generators are supplied by the confined charter.
export default {
  name: "create_todos",
  schema() {
    table("users").create({
      columns: {
        email: t.text().notNull().unique(),
        name: t.text().notNull(),
        handle: t.text().notNull().unique(),
      },
    });
    table("todos").create({
      columns: {
        userId: t.text().notNull().references("users", "id", { relation: "user" }),
        title: t.text().notNull(),
        priority: t.text().notNull().default("medium"),
        tags: t.json(),
        done: t.boolean().notNull().default(false),
        archived: t.boolean().notNull().default(false),
      },
      indexes: [{ name: "todos_user_idx", on: ["userId"] }],
    });
  },
};
