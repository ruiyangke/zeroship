import { enumType, table, t } from "@zeroship/migrate";
import { domain } from "@zeroship/migrate/pg";

const planTier = enumType("plan_tier");
const billingPeriod = domain("billing_period");

export function up() {
  planTier.create({ values: ["free", "pro"] });
  billingPeriod.create({
    as: t.integer(),
    check: (c) => c("VALUE").ge(1),
    default: 1,
    notNull: true,
  });

  table("subscriptions").create({
    columns: {
      tier: t.enum(planTier).notNull(),
      period: t.domain(billingPeriod),
    },
  });

  enumType("legacy_tier").create({ values: ["legacy"] }).drop({ ifExists: true });
  domain("legacy_period").drop({ ifExists: true });
}
