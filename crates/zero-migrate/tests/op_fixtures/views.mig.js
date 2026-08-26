import { view } from "zero-migrate";

export default {
  schema() {
    view("active_users", { schema: "app", columns: ["id", "email"] }).create({
      replace: true,
      as: (q) => q
        .from("users")
        .select(["id", "email"])
        .where((col) => col("deleted_at").isNull()),
    });

    view("recent_users").create({
      as: { raw: "SELECT id, email FROM users WHERE deleted_at IS NULL" },
    });

    view("old_users", { schema: "app" }).drop({ ifExists: true });
  },
};
