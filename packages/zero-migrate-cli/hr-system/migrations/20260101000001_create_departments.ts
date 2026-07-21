import { table, t, ids, now } from "zero-migrate";

// Departments are the organizational anchor. Public identity is a TypeID
// (prefix "dept"), so every reference downstream is format-checked. Uniqueness
// of `code` is enforced by a unique index (portable across all dialects) added
// in a later migration.
export const name = "create_departments";

export default {
  up() {
    table("departments").create({
      columns: {
        id: ids.typeId({ prefix: "dept" }).primaryKey(),
        code: t.char({ length: 8 }).notNull(),
        name: t.string({ length: 255 }).notNull(),
        is_active: t.boolean().notNull().default(true),
        created_at: t.timestamp().notNull().default(now()),
      },
    });
  },
};
