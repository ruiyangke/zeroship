import { table, t } from "zero-migrate";

// Denormalize a display name: add employees.full_name, then backfill it from
// first + last name with a resumable cursor over the primary key.
export const name = "backfill_full_name";

export default {
  up() {
    table("employees").column("full_name").add({ type: t.text() });

    table("employees").backfill({
      name: "backfill_employee_full_name",
      set: {
        full_name: (col) => col("first_name").concat(" ", col("last_name")),
      },
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 2,
    });
  },
};
