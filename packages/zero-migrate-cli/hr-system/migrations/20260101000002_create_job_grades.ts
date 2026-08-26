import { table, t } from "zero-migrate";

// Job grades use an explicit signed-64-bit key (imported from a legacy HRIS).
// `grade_code` uniqueness is a portable unique index (added later).
export const name = "create_job_grades";

export default {
  schema() {
    table("job_grades").create({
      columns: {
        id: t.bigInt().notNull(),
        grade_code: t.char({ length: 4 }).notNull(),
        min_salary: t.numeric({ precision: 12, scale: 2 }).notNull(),
        max_salary: t.numeric({ precision: 12, scale: 2 }).notNull(),
      },
      primaryKey: ["id"],
    });
  },
};
