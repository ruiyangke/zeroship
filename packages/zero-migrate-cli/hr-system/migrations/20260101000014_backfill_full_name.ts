import { table } from "zero-migrate";

// Denormalize a display name by backfilling the newly added employees.full_name
// from first + last name with a resumable cursor over the primary key.
export const name = "backfill_full_name";

export default {
  data() {
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
  inverse() {
    table("employees").backfill({
      name: "undo_backfill_employee_full_name",
      set: { full_name: null },
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 2,
    });
  },
};
