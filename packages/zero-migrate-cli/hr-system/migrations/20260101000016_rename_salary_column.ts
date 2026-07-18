import { table, t } from "zero-migrate";

// Clarify compensation semantics: base_salary -> annual_base_salary. On
// PostgreSQL this is an online expand/contract (leaves a pending contract to
// resolve); SQLite performs a table rebuild; MySQL renames in place.
export const name = "rename_salary_column";

export default {
  up() {
    table("employees")
      .column("base_salary")
      .rename({ to: "annual_base_salary", type: t.numeric({ precision: 12, scale: 2 }) });
  },
};
