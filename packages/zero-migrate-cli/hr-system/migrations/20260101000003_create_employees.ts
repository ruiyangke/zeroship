import { table, t, ids, now } from "zero-migrate";

// Employees carry a TypeID public key (prefix "emp"). The department reference
// is a format-matched TypeID FK; the grade reference is an int64 FK; manager_id
// is a nullable self-referential TypeID FK. Email uniqueness is a unique index
// added later (portable across all three dialects).
export const name = "create_employees";

export default {
  up() {
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
        email: t.text().notNull(),
        first_name: t.text().notNull(),
        last_name: t.text().notNull(),
        hire_date: t.date().notNull(),
        base_salary: t.numeric({ precision: 12, scale: 2 }).notNull(),
        employment_type: t.text().notNull(),
        status: t.text().notNull().default("active"),
        manager_id: ids
          .typeId({ prefix: "emp" })
          .references("employees", "id", { onDelete: "setNull" }),
        created_at: t.timestamp().notNull().default(now()),
      },
    });
  },
};
