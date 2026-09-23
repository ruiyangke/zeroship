"use server";
import { mutation } from "@zeroship/rpc/server";
import { z } from "zod";
import { demo, transact, changed } from "@gather/meal-kit/server/core";
import { requirePermission } from "@gather/meal-kit/server/staff-access";
import { expireReservations, settleAttempt } from "@gather/meal-kit/server/checkout-store";
import { event, wire } from "@gather/meal-kit/server/orders";
import { fail, marketSchema, type TimelineEvent } from "@gather/meal-kit/domain";

export const settleDemoPayment = mutation(
  async ({
    attemptId,
    outcome,
  }: {
    attemptId: string;
    outcome: "succeeded" | "failed";
  }) => {
    demo();
    return transact(async (tx) => {
      const attempt = await tx.meal_checkout_attempts.get(attemptId);
      if (!attempt)
        fail(
          /* i18n */ "Payment not found. Refresh the order and try again.",
          "NOT_FOUND",
          404,
        );
      const actor = await requirePermission(
        "preview",
        marketSchema.parse(attempt.market),
        tx,
      );
      const row = await settleAttempt(tx, attemptId, outcome);
      const event_key = `demo-payment:${attemptId}:${outcome}`;
      if (!(await tx.meal_events.get({ event_key })))
        await tx.meal_events.insert({
          event_key,
          kind: "demo_payment",
          payload: { actor: actor.id, attemptId, outcome },
        });
      return wire(row);
    });
  },
  {
    id: "gather.settleDemoPayment",
    input: z.object({
      attemptId: z.string(),
      outcome: z.enum(["succeeded", "failed"]),
    }),
  },
);

export const expireDemoCheckout = mutation(
  async ({ attemptId }: { attemptId: string }) => {
    demo();
    return transact(async (tx) => {
      const attempt = await tx.meal_checkout_attempts.get(attemptId);
      if (!attempt)
        fail(
          /* i18n */ "Payment not found. Refresh the order and try again.",
          "NOT_FOUND",
          404,
        );
      const actor = await requirePermission(
        "preview",
        marketSchema.parse(attempt.market),
        tx,
      );
      if (attempt.reservation === "active") {
        changed(
          await tx.meal_checkout_attempts.update(
            { id: attempt.id, version: attempt.version },
            { expires_at: new Date().toISOString() },
          ),
        );
        await tx.meal_events.insert({
          event_key: `demo-expiry:${attemptId}`,
          kind: "demo_expiry",
          payload: { actor: actor.id, attemptId },
        });
      }
      await expireReservations(tx, attempt.market);
      return wire((await tx.meal_orders.get(attempt.order_id))!);
    });
  },
  {
    id: "gather.expireDemoCheckout",
    input: z.object({ attemptId: z.string() }),
  },
);

export const sweepCheckouts = mutation(
  async ({ market }: { market: z.infer<typeof marketSchema> }) => {
    return transact(async (tx) => {
      await requirePermission("preview", market, tx);
      return { released: await expireReservations(tx, market) };
    });
  },
  { id: "gather.sweepCheckouts", input: z.object({ market: marketSchema }) },
);

export const refundRecoveredPayment = mutation(
  async ({ id }: { id: string }) => {
    demo();
    return transact(async (tx) => {
      const row = await tx.meal_orders.get(id);
      if (!row) fail(/* i18n */ "Order not found.", "NOT_FOUND", 404);
      const actor = await requirePermission(
        "refund",
        marketSchema.parse(row.market),
        tx,
      );
      if (row.status === "canceled" && row.refunded === row.total)
        return wire(row);
      if (row.status !== "payment_recovery" || row.payment !== "succeeded")
        fail(
          /* i18n */ "This order does not have a late payment to refund.",
          "INVALID_STATE",
          409,
        );
      const updated = changed(
        await tx.meal_orders.update(
          { id, version: row.version },
          {
            status: "canceled",
            refunded: row.total,
            timeline: [
              ...(row.timeline as TimelineEvent[]),
              event(
                "late_payment_refunded",
                /* i18n */ "Your late payment has been refunded. This box will not be delivered.",
              ),
            ],
          },
        ),
      );
      await tx.meal_events.insert({
        event_key: `late-refund:${id}`,
        kind: "demo_refund",
        payload: {
          actor: actor.id,
          orderId: id,
          amount: row.total - row.refunded,
        },
      });
      return wire(updated);
    });
  },
  { id: "gather.refundRecoveredPayment", input: z.object({ id: z.string() }) },
);
