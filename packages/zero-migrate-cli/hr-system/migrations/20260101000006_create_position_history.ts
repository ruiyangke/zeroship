import { table, t, ids } from "@zeroship/migrate";

// Position history is a temporal join: composite primary key on
// (employee_id, effective_from), with format-matched TypeID and ULID FKs.
export const name = "create_position_history";

export default {
  schema() {
    table("employee_position_history").create({
      columns: {
        employee_id: ids
          .typeId({ prefix: "emp" })
          .notNull()
          .references("employees", "id", { onDelete: "cascade" }),
        position_id: ids
          .ulid()
          .notNull()
          .references("positions", "id", { onDelete: "restrict" }),
        effective_from: t.calendarDate().notNull(),
        effective_to: t.calendarDate(),
      },
      primaryKey: ["employee_id", "effective_from"],
    });
  },
};
