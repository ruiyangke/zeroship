import { table } from "zero-migrate";

// A routine HR maintenance migration: mark a departing employee terminated, and
// purge rejected leave requests. Exercises predicated UPDATE and DELETE DML.
export const name = "adjust_compensation";

export default {
  up() {
    table("employees").update({
      set: { status: "terminated" },
      where: (col) => col("email").eq("grace@example.com"),
    });

    table("leave_requests").delete({
      where: (col) => col("status").eq("rejected"),
    });
  },
};
