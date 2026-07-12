import { enumType, table, t, domain } from "zero-migrate";

const planTier = enumType("plan_tier");
const billingPeriod = domain("billing_period");
const accountState = domain("account_state");

export function up() {
  planTier.create({ values: ["free", "pro"] });
  billingPeriod.create({
    as: t.int(),
    check: (v) => v.ge(1),
    default: 1,
    notNull: true,
  });
  accountState.create({
    as: t.text(),
    check: (v) => v.in(["active", "past_due"]),
  });

  table("subscriptions").create({
    columns: {
      tier: t.enum(planTier).notNull(),
      period: t.domain(billingPeriod),
    },
  });

  table("pg_expr_checks").create({
    columns: {
      status: t.text().notNull(),
      name: t.text().notNull(),
      data: t.json().notNull(),
    },
    checks: [
      { name: "status_ne_all", expr: (col) => col("status").notIn(["x"]) },
      { name: "name_shape", expr: (col) => col("name").regex("^[a-z]+$") },
      { name: "data_size", expr: (col) => col("data").columnSize().le(8192) },
    ],
  });

  enumType("legacy_tier").create({ values: ["legacy"] }).drop({ ifExists: true });
  domain("legacy_period").drop({ ifExists: true });
}
