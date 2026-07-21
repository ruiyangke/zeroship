import { table, t, ids } from "zero-migrate";

// Payroll: runs (ULID key) and per-employee items with a composite primary key
// and a STORED generated net_pay column (gross - tax). Period uniqueness is a
// unique index added later.
export const name = "create_payroll";

export default {
  up() {
    table("payroll_runs").create({
      columns: {
        id: ids.ulid().primaryKey(),
        period_label: t.char({ length: 7 }).notNull(),
        run_date: t.date().notNull(),
        status: t.string({ length: 32 }).notNull().default("draft"),
      },
    });

    table("payroll_items").create({
      columns: {
        run_id: ids
          .ulid()
          .notNull()
          .references("payroll_runs", "id", { onDelete: "cascade" }),
        employee_id: ids
          .typeId({ prefix: "emp" })
          .notNull()
          .references("employees", "id", { onDelete: "restrict" }),
        gross_pay: t.numeric({ precision: 14, scale: 2 }).notNull(),
        tax: t.numeric({ precision: 14, scale: 2 }).notNull(),
        net_pay: t
          .numeric({ precision: 14, scale: 2 })
          .generated((col) => col("gross_pay").sub(col("tax"))),
      },
      primaryKey: ["run_id", "employee_id"],
    });
  },
};
