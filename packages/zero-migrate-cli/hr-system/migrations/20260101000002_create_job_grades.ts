import { table, t } from "@zeroship/migrate";

// Job grades use an explicit signed-64-bit key (imported from a legacy HRIS).
// `grade_code` uniqueness is a portable unique index (added later).
export const name = "create_job_grades";

export default {
  schema() {
    table("job_grades").create({
      columns: {
        id: t.bigInt().required(),
        grade_code: t.char({ length: 4 }).required(),
        min_salary: t.numeric({ precision: 12, scale: 2 }).required(),
        max_salary: t.numeric({ precision: 12, scale: 2 }).required(),
      },
      primaryKey: ["id"],
    });
  },
};
