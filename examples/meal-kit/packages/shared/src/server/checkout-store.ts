import type { Id } from "@zeroship/db";
import { checkoutDeadline, reservationActive } from "../checkout-domain";
import { cartSchema, fail, type OrderSnapshot } from "../domain";
import { changed, type Tx } from "./core";
import { adjustStock, event, type OrderRow } from "./orders";

export async function requirePaymentReady(
  tx: Tx,
  owner: string,
  market: string,
) {
  // An attempt blocks only while its RESERVATION is live. `expireReservations`
  // releases the stock, moves the order to `checkout_expired` and tells the
  // shopper their meals are no longer reserved - but it leaves `payment` at
  // whatever the provider last said, because that is a record rather than a
  // lifecycle. Blocking on `payment` alone therefore locked a customer out of
  // the market forever: the one attempt nobody can settle any more is the one
  // the refusal pointed them at. `reservationActive` is the single definition
  // of live, so this reuses it instead of restating the deadline here.
  const unfinished = (
    await tx.meal_checkout_attempts.find({
      owner_id: owner,
      market,
      reservation: "active",
      payment: { $in: ["processing", "requires_action"] },
    })
  ).filter((attempt) => reservationActive(attempt));
  if (unfinished.length)
    fail(
      /* i18n */ "You have a payment in progress. Open My deliveries to finish it before starting another order.",
      "PAYMENT_PENDING",
      409,
    );
}

export async function activatePlan(tx: Tx, row: NonNullable<OrderRow>) {
  const snapshot = row.snapshot as OrderSnapshot;
  if (!snapshot.cart.recurring) return;
  const existing = await tx.meal_plans.get({
    owner_id: row.owner_id,
    market: row.market,
  });
  const next = new Date(`${snapshot.cart.deliveryDate}T12:00:00Z`);
  next.setUTCDate(next.getUTCDate() + 7);
  const values = {
    status: "active",
    configuration: {
      cart: snapshot.cart,
      address: snapshot.address,
      quote: snapshot.quote,
      consentAt: snapshot.consentAt,
    },
    next_date: next.toISOString().slice(0, 10),
  };
  if (existing)
    changed(
      await tx.meal_plans.update(
        { id: existing.id, version: existing.version },
        values,
      ),
    );
  else
    await tx.meal_plans.insert({
      ...values,
      owner_id: row.owner_id,
      market: row.market,
      skipped: [],
    });
}

export async function expireReservations(
  tx: Tx,
  market: string,
  now = Date.now(),
) {
  const attempts = await tx.meal_checkout_attempts.find({
    market,
    reservation: "active",
    expires_at: { $lte: new Date(now).toISOString() },
  });
  for (const attempt of attempts) {
    await adjustStock(tx, cartSchema.parse(attempt.cart), "release");
    changed(
      await tx.meal_checkout_attempts.update(
        { id: attempt.id, version: attempt.version },
        { reservation: "expired" },
      ),
    );
    const row = await tx.meal_orders.get(attempt.order_id);
    if (
      row &&
      (row.snapshot as OrderSnapshot).paymentAttemptId === attempt.id &&
      row.status === "pending_payment"
    ) {
      changed(
        await tx.meal_orders.update(
          { id: row.id, version: row.version },
          {
            status: "checkout_expired",
            timeline: [
              ...(row.timeline as ReturnType<typeof event>[]),
              event(
                "checkout_expired",
                /* i18n */ "The time to complete payment has ended. Your meals are no longer reserved.",
              ),
            ],
          },
        ),
      );
    }
  }
  return attempts.length;
}

export async function startAttempt(
  tx: Tx,
  row: NonNullable<OrderRow>,
  payment: "processing" | "requires_action",
  now = Date.now(),
) {
  const snapshot = row.snapshot as OrderSnapshot;
  const expires_at = checkoutDeadline(snapshot.cutoff, now);
  await adjustStock(tx, snapshot.cart, "reserve");
  const attempt = await tx.meal_checkout_attempts.insert({
    order_id: row.id as Id<"meal_orders">,
    owner_id: row.owner_id,
    market: row.market,
    payment,
    reservation: "active",
    expires_at,
    cart: snapshot.cart,
    total: row.total,
    currency: snapshot.quote.currency,
  });
  return changed(
    await tx.meal_orders.update(
      { id: row.id, version: row.version },
      {
        status: "pending_payment",
        payment,
        snapshot: {
          ...snapshot,
          paymentAttemptId: attempt.id,
          paymentDeadline: expires_at,
        },
      },
    ),
  );
}

export async function settleAttempt(
  tx: Tx,
  attemptId: string,
  outcome: "succeeded" | "failed",
  now = Date.now(),
) {
  const found = await tx.meal_checkout_attempts.get(attemptId);
  if (!found)
    fail(
      /* i18n */ "Payment not found. Refresh the order and try again.",
      "NOT_FOUND",
      404,
    );
  await expireReservations(tx, found.market, now);
  const attempt = (await tx.meal_checkout_attempts.get(attemptId))!;
  const row = (await tx.meal_orders.get(attempt.order_id))!;
  const snapshot = row.snapshot as OrderSnapshot;
  if (attempt.payment === outcome) return row;
  if (["succeeded", "failed"].includes(attempt.payment))
    fail(
      /* i18n */ "This payment already has a final result. Refresh the order.",
      "PAYMENT_FINAL",
      409,
    );
  if (
    snapshot.paymentAttemptId !== attempt.id ||
    row.total !== attempt.total ||
    snapshot.quote.currency !== attempt.currency
  )
    fail(
      /* i18n */ "This payment needs review. Please contact support.",
      "PAYMENT_MISMATCH",
      409,
    );
  const reserved = reservationActive(attempt, now);
  if (outcome === "failed" && reserved)
    await adjustStock(tx, snapshot.cart, "release");
  changed(
    await tx.meal_checkout_attempts.update(
      { id: attempt.id, version: attempt.version },
      {
        payment: outcome,
        reservation: reserved
          ? outcome === "succeeded"
            ? "consumed"
            : "released"
          : attempt.reservation,
      },
    ),
  );
  const status =
    outcome === "succeeded"
      ? reserved && row.status !== "canceled"
        ? "confirmed"
        : "payment_recovery"
      : row.status === "canceled"
        ? "canceled"
        : "pending_payment";
  const updated = changed(
    await tx.meal_orders.update(
      { id: row.id, version: row.version },
      {
        status,
        payment: outcome,
        timeline: [
          ...(row.timeline as ReturnType<typeof event>[]),
          event(
            status === "payment_recovery"
              ? "late_payment"
              : `payment_${outcome}`,
            status === "payment_recovery"
              ? /* i18n */ "Payment arrived after your meals were released. Your order needs review before it can be prepared."
              : outcome === "succeeded"
                ? /* i18n */ "Payment confirmed"
                : /* i18n */ "Payment was declined. No payment was taken.",
          ),
        ],
      },
    ),
  );
  if (status === "confirmed") await activatePlan(tx, updated);
  return updated;
}

export async function releasePendingAttempt(
  tx: Tx,
  row: NonNullable<OrderRow>,
) {
  const id = (row.snapshot as OrderSnapshot).paymentAttemptId;
  if (!id) return;
  const attempt = await tx.meal_checkout_attempts.get(id);
  if (!attempt || !["processing", "requires_action"].includes(attempt.payment))
    return;
  if (attempt.reservation === "active")
    await adjustStock(tx, cartSchema.parse(attempt.cart), "release");
  changed(
    await tx.meal_checkout_attempts.update(
      { id: attempt.id, version: attempt.version },
      {
        reservation:
          attempt.reservation === "active" ? "released" : attempt.reservation,
        payment: "canceled",
      },
    ),
  );
}
