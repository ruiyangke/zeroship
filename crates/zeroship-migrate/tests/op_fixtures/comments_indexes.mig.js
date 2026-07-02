// op.* migration fixture — structured COMMENT ON plus closed index elements.
// Covers expression index elements, partial-index predicates, and representative
// comment targets through both fluent handles and the top-level closed target API.
import { comment, enumType, table, t, view } from "@zeroship/migrate";
import { sequence } from "@zeroship/migrate/pg";

export const name = "comments_indexes";

const userStatus = enumType("user_status");

export function up() {
  userStatus.create({ values: ["active", "disabled"] });
  sequence("users_seq").create();

  table("users").create({
    columns: {
      id: t.uuid().notNull(),
      email: t.text().notNull(),
      active: t.boolean().notNull().default(true),
      status: t.enum(userStatus),
    },
    uniques: [{ name: "users_email_uq", columns: ["email"] }],
  });

  table("users").index("users_email_lower_idx").add({
    columns: ["email", { kind: "expr", expr: (c) => c.fn.lower(c("email")) }],
    where: (c) => c("active").isTrue(),
  });

  view("active_users", { columns: ["id", "email"] }).create({
    as: (q) => q
      .from("users")
      .select(["id", "email"])
      .where((c) => c("active").isTrue()),
  });

  table("users").comment("End-user accounts");
  table("users").column("email").comment("Login email");
  table("users").constraint("users_email_uq").comment("Email uniqueness");
  table("users").index("users_email_lower_idx").comment("Case-insensitive email lookup");
  view("active_users").comment("Currently active users");
  userStatus.comment("Allowed user states");
  sequence("users_seq").comment("Synthetic user sequence");
  comment({ kind: "function", name: "normalize_email" }, "Unambiguous function comment");
  comment({ kind: "table", name: "users" }, null);
}
