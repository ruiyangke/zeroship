import { table, t } from "@zeroship/migrate";

// The seven platform system columns (id, created_at, updated_at, created_by,
// updated_by, version, deleted_at) are INJECTED by the confined charter, not
// declared here. Declaring `id` yourself collides with the injected column and
// the descriptor is refused - see docs/reference/db.md on reserved field names.
//
// `t.ref()` is not in the vendored DSL; a native foreign key is spelled
// `t.text().references(table, column)`.
export default {
  name: "create_todos",
  up() {
    table("users").create({
      columns: {
        email: t.text().notNull().unique(),
        name: t.text().notNull(),
        handle: t.text().notNull().unique(),
      },
    });
    table("todos").create({
      columns: {
        userId: t.text().notNull().references("users", "id"),
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
