import { pgDomain, pgEnum, table, t } from "@zeroship/migrate";

const planTier = pgEnum("plan_tier", ["free", "pro"]);
const billingPeriod = pgDomain("billing_period").create({
  as: t.integer(),
  check: (c) => c("VALUE").ge(1),
  default: 1,
  notNull: true,
});

export function up() {
  table("subscriptions").create({
    columns: {
      tier: t.enum(planTier).notNull(),
      period: t.domain(billingPeriod),
    },
  });

  pgEnum("legacy_tier", ["legacy"]).drop({ ifExists: true });
  pgDomain("legacy_period").drop({ ifExists: true });
}
