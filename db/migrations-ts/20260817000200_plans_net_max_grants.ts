import { table } from "@zeroship/migrate";

export const name = "plans_net_max_grants";

// `zeroship.plans.net_policy_limits_json` gained a third key, `max_grants`:
// the number of `app_net_grants` rows an app may hold. It exists because the
// grant author is now the creator (`/api/apps/{id}/net-grants`), and neither
// `max_sockets` nor `egress_ceiling_bytes` bounds how wide a destination set a
// creator can enumerate one exact host at a time.
//
// This moves the COLUMN DEFAULT only. It does not rewrite existing rows: the
// Rust side reads the column through `AppNetPolicyLimits`, whose `max_grants`
// carries `#[serde(default)]` resolving to the free-tier 10, so a row written
// before this migration caps at the tightest tier rather than at "unbounded".
// The three built-in tiers re-upsert their full JSON on every control boot
// (`plan_catalog::seed_plans`), so they pick the new value up without a
// backfill; an operator-authored custom plan keeps the fail-closed default
// until its next PUT.
//
// A NEW migration rather than an edit to 20260702000400: that one is applied
// and checksummed, so editing it in place would read as drift.
export function up() {
  table("plans", { schema: "zeroship" })
    .column("net_policy_limits_json")
    .setDefault({ max_sockets: 4, egress_ceiling_bytes: 10485760, max_grants: 10 });
}
