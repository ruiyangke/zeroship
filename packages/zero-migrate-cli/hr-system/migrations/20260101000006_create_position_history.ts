import { table, t } from "@zeroship/migrate";

// Position history is a temporal join: composite primary key on
// (employee_id, effective_from), with a format-matched TypeID FK to employees
// and a bounded-string FK to positions.
export const name = "create_position_history";

export default {
  schema() {
    table("employee_position_history").create({
      columns: {
        employee_id: t
          .typedId("emp")
          .required()
          .references("employees", "id", { onDelete: "cascade" }),
        position_id: t
          .string({ length: 26 })
          .required()
          .references("positions", "id", { onDelete: "restrict" }),
        effective_from: t.calendarDate().required(),
        effective_to: t.calendarDate(),
      },
      primaryKey: ["employee_id", "effective_from"],
    });
  },
};
