"use server";

// The back office's server surface: everything the staff running the business
// do.
//
// Every one of these reads and writes `env.db`, which the workspace config
// binds to `databases.main` - the same database the storefront binds. The
// orders on this board were written by customers in the storefront, in that
// database, and the menus published here are what the storefront sells.

import { env } from "zeroship";
import { auth } from "@zeroship/auth";
import { mutation, query } from "@zeroship/rpc/server";
import { z } from "zod";
import {
  deliveryDates,
  type MarketId,
} from "@gather/meal-kit/catalog";
import {
  marketSchema,
  fail,
  checkTransition,
  type TimelineEvent,
} from "@gather/meal-kit/domain";
import {
  must,
  user,
  transact,
  changed,
  demo,
} from "@gather/meal-kit/server/core";
import {
  staffAccess,
  requirePermission,
  forbidden,
} from "@gather/meal-kit/server/staff-access";
import { allows, canUseWorkspace } from "@gather/meal-kit/staff-domain";
import { sellableMenu } from "@gather/meal-kit/server/catalog-store";
import { expireReservations } from "@gather/meal-kit/server/checkout-store";
import { event, wire } from "@gather/meal-kit/server/orders";

export const getSession = query(
  async () => {
    const u = auth.getUser();
    return {
      user: u
        ? { id: u.id, name: u.name ?? "Guest", email: u.email ?? "" }
        : null,
      staff: u ? await staffAccess(u.id) : null,
      mode: "demo" as const,
    };
  },
  { id: "gather.session" },
);
export const getOperations = query(
  async ({ market }: { market: MarketId }) => {
    const access = await staffAccess(user().id);
    if (!canUseWorkspace(access, market)) forbidden();
    await transact((tx) => expireReservations(tx, market));
    const orders = allows(access, "orders", market)
      ? must(await env.db.meal_orders.find({ market }).sort({ created_at: -1 }))
      : [];
    const stock = allows(access, "inventory", market)
      ? must(await env.db.meal_inventory.find({ market }))
      : [];
    const cases = allows(access, "support", market)
      ? must(await env.db.meal_cases.find({}))
      : [];
    const recipeRows = stock.length
      ? must(await env.db.meal_recipes.find({}))
      : [];
    return {
      access: access!,
      orders: orders.map(wire),
      stock,
      recipeNames: recipeRows.map((row) => {
        const recipe = row.draft as import("@gather/meal-kit/catalog-domain").RecipeDraft;
        return {
          id: row.slug,
          en: recipe.name,
          zh: recipe.translations.zh.name,
        };
      }),
      cases: cases.filter((c) => orders.some((o) => o.id === c.order_id)),
    };
  },
  { id: "gather.operations", input: z.object({ market: marketSchema }) },
);
export const prepareMenu = mutation(
  async ({ market, date }: { market: MarketId; date: string }) => {
    demo();
    if (!deliveryDates(market).includes(date))
      fail(/* i18n */ "Choose an available delivery date.");
    return transact(async (tx) => {
      await requirePermission("inventory", market, tx);
      const menu = await sellableMenu(tx, market, date);
      for (const recipe_id of [...menu.recipes.map((r) => r.id), "delivery"]) {
        const stock_key = `${market}:${date}:${recipe_id}`;
        if (!(await tx.meal_inventory.get({ stock_key })))
          await tx.meal_inventory.insert({
            stock_key,
            market,
            recipe_id,
            available: 0,
            published: true,
          });
      }
      return { prepared: true };
    });
  },
  {
    id: "gather.prepareMenu",
    input: z.object({ market: marketSchema, date: z.string() }),
  },
);
export const advanceOrder = mutation(
  async ({ id, next }: { id: string; next: string }) => {
    demo();
    return transact(async (tx) => {
      const row = await tx.meal_orders.get(id);
      if (!row) fail(/* i18n */ "Order not found.", "NOT_FOUND", 404);
      await requirePermission(
        "fulfillment",
        marketSchema.parse(row.market),
        tx,
      );
      const order = wire(row);
      checkTransition(order, next);
      const updated = changed(
        await tx.meal_orders.update(
          { id, version: row.version },
          {
            fulfillment: next,
            status: next === "delivered" ? "completed" : order.status,
            timeline: [
              ...order.timeline,
              event(next, /* i18n */ "Delivery status updated"),
            ],
          },
        ),
      );
      return wire(updated);
    });
  },
  {
    id: "gather.advance",
    input: z.object({
      id: z.string(),
      next: z.enum([
        "packing",
        "packed",
        "dispatched",
        "delivered",
        "exception",
      ]),
    }),
  },
);
export const setInventory = mutation(
  async ({
    stockKey,
    available,
    published,
  }: {
    stockKey: string;
    available: number;
    published: boolean;
  }) => {
    return transact(async (tx) => {
      const row = await tx.meal_inventory.get({ stock_key: stockKey });
      if (!row) fail(/* i18n */ "Inventory not found.", "NOT_FOUND", 404);
      await requirePermission("inventory", marketSchema.parse(row.market), tx);
      return changed(
        await tx.meal_inventory.update(
          { id: row.id, version: row.version },
          { available, published },
        ),
      );
    });
  },
  {
    id: "gather.inventory",
    input: z.object({
      stockKey: z.string(),
      available: z.number().int().min(0).max(10000),
      published: z.boolean(),
    }),
  },
);
export const resolveIssue = mutation(
  async ({
    id,
    resolution,
    refund,
    requestKey,
  }: {
    id: string;
    resolution: string;
    refund: number;
    requestKey: string;
  }) => {
    demo();
    return transact(async (tx) => {
      const issue = await tx.meal_cases.get(id);
      if (!issue) fail(/* i18n */ "Case not found.", "NOT_FOUND", 404);
      const order = await tx.meal_orders.get(issue.order_id);
      if (!order) fail(/* i18n */ "Order not found.");
      await requirePermission("support", marketSchema.parse(order.market), tx);
      if (refund > 0)
        await requirePermission("refund", marketSchema.parse(order.market), tx);
      if (issue.status === "resolved") return issue;
      if (
        refund > order.total - order.refunded ||
        (refund > 0 && order.payment !== "succeeded")
      )
        fail(
          /* i18n */ "Refund exceeds the remaining paid amount.",
          "OVER_REFUND",
          409,
        );
      const existing = await tx.meal_events.get({ event_key: requestKey });
      if (existing)
        fail(/* i18n */ "Resolution key already used.", "CONFLICT", 409);
      await tx.meal_events.insert({
        event_key: requestKey,
        kind: "support_resolution",
        payload: { id, refund },
      });
      await tx.meal_orders.update(
        { id: order.id, version: order.version },
        {
          refunded: order.refunded + refund,
          timeline: [
            ...(order.timeline as TimelineEvent[]),
            event("support_resolved", resolution),
          ],
        },
      );
      return tx.meal_cases.update(
        { id, version: issue.version },
        { status: "resolved", resolution },
      );
    });
  },
  {
    id: "gather.resolve",
    input: z.object({
      id: z.string(),
      resolution: z.string().min(5).max(500),
      refund: z.number().int().min(0),
      requestKey: z.string().uuid(),
    }),
  },
);
export const setPaymentScenario = mutation(
  async ({
    customerId,
    market,
    outcome,
  }: {
    customerId: string;
    market: MarketId;
    outcome: "succeeded" | "failed" | "requires_action" | "processing";
  }) => {
    demo();
    return transact(async (tx) => {
      const actor = await requirePermission("preview", market, tx);
      const event_key = `scenario:${customerId}:${market}`;
      const existing = await tx.meal_events.get({ event_key });
      const payload = {
        outcome,
        expiresAt: Date.now() + 30 * 60_000,
        actor: actor.id,
      };
      if (existing)
        changed(
          await tx.meal_events.update(
            { id: existing.id, version: existing.version },
            { payload },
          ),
        );
      else
        await tx.meal_events.insert({
          event_key,
          kind: "demo_scenario",
          payload,
        });
      return { saved: true };
    });
  },
  {
    id: "gather.paymentScenario",
    input: z.object({
      customerId: z.string().min(1).max(100),
      market: marketSchema,
      outcome: z.enum(["succeeded", "failed", "requires_action", "processing"]),
    }),
  },
);

export {
  getCatalogWorkspace,
  saveRecipeDraft,
  approveRecipe,
  archiveRecipe,
  saveMenuDraft,
  publishMenu,
  withdrawMenu,
} from "./server/catalog";
export { loadSampleMenus } from "./server/demo-catalog";
export { getRecipeFeedback } from "./server/cooking";
export { getStaffTeam, saveStaffMember } from "./server/staff";
export {
  settleDemoPayment,
  expireDemoCheckout,
  sweepCheckouts,
  refundRecoveredPayment,
} from "./server/checkout";
