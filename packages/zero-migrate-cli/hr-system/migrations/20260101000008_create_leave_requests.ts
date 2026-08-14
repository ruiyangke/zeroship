import { table, t, ids, uuidV4, now } from "zero-migrate";

// Leave requests use a database-generated UUIDv4 key (portable across all three
// dialects; UUIDv7 DB-generation is PostgreSQL-18-only), a format-matched
// TypeID FK to the employee, and a JSON metadata bag with an empty-object
// default.
export const name = "create_leave_requests";

export default {
  schema() {
    table("leave_requests").create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        employee_id: ids
          .typeId({ prefix: "emp" })
          .notNull()
          .references("employees", "id", { onDelete: "cascade" }),
        leave_type: t.string({ length: 32 }).notNull(),
        start_date: t.date().notNull(),
        end_date: t.date().notNull(),
        days: t.numeric({ precision: 4, scale: 1 }).notNull(),
        // `status` is an index member (composite employee/status index), so it is
        // a bounded `t.string`, not unbounded `t.text()`.
        status: t.string({ length: 32 }).notNull().default("pending"),
        metadata: t.json().notNull().default({}),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
  },
};
