import { table, t, ids, now } from "zero-migrate";

// Employees carry a TypeID public key (prefix "emp"). The department reference
// is a format-matched TypeID FK; the grade reference is an int64 FK; manager_id
// is a nullable self-referential TypeID FK. Email uniqueness is a unique index
// added later (portable across all three dialects).
export const name = "create_employees";

export default {
  schema() {
    table("employees").create({
      columns: {
        id: ids.typeId({ prefix: "emp" }).primaryKey(),
        dept_id: ids
          .typeId({ prefix: "dept" })
          .notNull()
          .references("departments", "id", { onDelete: "restrict" }),
        grade_id: t
          .bigInt()
          .notNull()
          .references("job_grades", "id", { onDelete: "restrict" }),
        // Bounded strings: `email` and `status` are index members (a unique email
        // index and the composite dept/status index), so they must be `t.string`
        // (VARCHAR) — MySQL cannot index unbounded `t.text()`. The rest are bounded
        // by nature (names, a short employment-type vocabulary).
        email: t.string({ length: 254 }).notNull(),
        first_name: t.string({ length: 255 }).notNull(),
        last_name: t.string({ length: 255 }).notNull(),
        hire_date: t.date().notNull(),
        base_salary: t.numeric({ precision: 12, scale: 2 }).notNull(),
        employment_type: t.string({ length: 32 }).notNull(),
        status: t.string({ length: 32 }).notNull().default("active"),
        manager_id: ids
          .typeId({ prefix: "emp" })
          .references("employees", "id", { onDelete: "setNull" }),
        created_at: t.timestamp().notNull().default(now()),
      },
    });
  },
};
