import { z } from "zod";
import { cutoffForDate, type MarketId } from "@gather/meal-kit/catalog";
import { cartSchema, fail } from "@gather/meal-kit/domain";
import { cookingUnitsSchema } from "@gather/meal-kit/cooking-domain";

export const cuisines = [
  "mediterranean",
  "east_asian",
  "south_asian",
  "middle_eastern",
  "american",
] as const;
export const kitchenEquipment = [
  "oven",
  "hob",
  "microwave",
  "blender",
] as const;
export const preferencesSchema = z.object({
  favorites: z.array(z.string().min(1).max(100)).max(50).default([]),
  exclude: cartSchema.shape.exclude,
  cuisines: z.array(z.enum(cuisines)).max(cuisines.length).default([]),
  equipment: z
    .array(z.enum(kitchenEquipment))
    .max(kitchenEquipment.length)
    .default([]),
  units: cookingUnitsSchema.default("metric"),
  marketing: z.boolean().default(false),
});
export type Preferences = z.infer<typeof preferencesSchema>;

export function nextOpenCycle(
  anchor: string,
  market: MarketId,
  now = Date.now(),
): string {
  const date = Date.parse(`${anchor}T12:00:00Z`);
  if (
    !/^\d{4}-\d{2}-\d{2}$/.test(anchor) ||
    !Number.isFinite(date) ||
    new Date(date).toISOString().slice(0, 10) !== anchor
  )
    fail(/* i18n */ "Choose an available delivery date.");
  const cutoff = Date.parse(cutoffForDate(market, anchor));
  let weeks = Math.max(0, Math.floor((now - cutoff) / (7 * 86_400_000)));
  let candidate = new Date(date + weeks * 7 * 86_400_000)
    .toISOString()
    .slice(0, 10);
  while (Date.parse(cutoffForDate(market, candidate)) <= now) {
    weeks++;
    candidate = new Date(date + weeks * 7 * 86_400_000)
      .toISOString()
      .slice(0, 10);
  }
  return candidate;
}

export function planChange(
  plan: { status: string; next_date: string; skipped: unknown },
  action: "pause" | "resume" | "cancel" | "skip",
  market: MarketId,
  now = Date.now(),
) {
  if (plan.status === "canceled")
    fail(
      /* i18n */ "Choose a new box to restart weekly deliveries.",
      "PLAN_CANCELED",
      409,
    );
  if (action === "skip" && plan.status !== "active")
    fail(
      /* i18n */ "Resume your plan before skipping a delivery.",
      "PLAN_INACTIVE",
      409,
    );
  const current = nextOpenCycle(plan.next_date, market, now);
  const following = new Date(
    Date.parse(`${current}T12:00:00Z`) + 7 * 86_400_000,
  )
    .toISOString()
    .slice(0, 10);
  return {
    status:
      action === "pause"
        ? "paused"
        : action === "cancel"
          ? "canceled"
          : "active",
    next_date: action === "skip" ? following : current,
    skipped:
      action === "skip"
        ? [...new Set([...(plan.skipped as string[]), current])]
        : z.array(z.string()).parse(plan.skipped),
  };
}
