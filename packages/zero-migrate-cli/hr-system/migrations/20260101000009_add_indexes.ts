import { table } from "zero-migrate";

// Uniqueness and access-path indexes. Unique indexes carry the business keys
// (department code, grade code, employee email, position title, payroll
// period); composite btree indexes serve the hottest lookups. All are portable
// and render identically on PostgreSQL, MySQL, and SQLite.
export const name = "add_indexes";

export default {
  schema() {
    table("departments").index("uq_departments_code").add({ on: ["code"], unique: true });
    table("job_grades").index("uq_job_grades_code").add({ on: ["grade_code"], unique: true });
    table("employees").index("uq_employees_email").add({ on: ["email"], unique: true });
    table("positions").index("uq_positions_title").add({ on: ["title"], unique: true });
    table("payroll_runs").index("uq_payroll_runs_period").add({ on: ["period_label"], unique: true });

    table("employees").index("ix_employees_dept_status").add({ on: ["dept_id", "status"] });
    table("leave_requests").index("ix_leave_employee_status").add({ on: ["employee_id", "status"] });
  },
};
