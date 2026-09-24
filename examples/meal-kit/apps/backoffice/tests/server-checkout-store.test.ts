// `packages/shared/src/server/orders.ts` and `checkout-store.ts` - the stock
// arithmetic and payment settlement both apps run against the one database.
// The storefront reaches these when a customer pays; the back office reaches
// the same functions when an operator simulates a settlement, so a change here
// moves both apps at once.

import { describe, expect, test } from "vitest";
import { memoryTx, refusal, type MemoryRow } from "./fixtures/tx";
import {
  adjustStock,
  wire,
  event,
  type OrderRow,
} from "@gather/meal-kit/server/orders";
import {
  activatePlan,
  expireReservations,
  releasePendingAttempt,
  requirePaymentReady,
  settleAttempt,
  startAttempt,
} from "@gather/meal-kit/server/checkout-store";
import {
  defaultCart,
  type Cart,
  type OrderSnapshot,
} from "@gather/meal-kit/domain";
import { cutoffForDate, deliveryDates, markets } from "@gather/meal-kit/catalog";

type Db = ReturnType<typeof memoryTx>;

// A seeded row standing in for a stored one. The fixture's tables hold the
// columns these functions read; this names the substitution once instead of
// repeating it at every call that hands a row to a subject.
const stored = (row: MemoryRow) => row as unknown as NonNullable<OrderRow>;

const date = deliveryDates("us")[0];
const cutoff = cutoffForDate("us", date);
const owner = "pws_gathercustomer000001";
const box = (over: Partial<Cart> = {}): Cart => ({
  ...defaultCart("us"),
  postal: "10001",
  deliveryDate: date,
  mealCount: 2,
  servings: 2,
  recurring: false,
  recipeIds: ["lemon-chicken", "pesto-pasta"],
  ...over,
});

function stocked(db: Db, cart: Cart, available = 10) {
  for (const recipeId of [...cart.recipeIds, "delivery"])
    db.table("meal_inventory").seed({
      stock_key: `${cart.market}:${cart.deliveryDate}:${recipeId}`,
      market: cart.market,
      recipe_id: recipeId,
      available,
      published: true,
    });
}
const held = (db: Db, key: string) =>
  db.table("meal_inventory").rows.find((row) => row.stock_key === key)!
    .available;

function snapshot(cart: Cart, over: Partial<OrderSnapshot> = {}): OrderSnapshot {
  return {
    cart,
    address: {
      country: "US",
      province: "NY",
      district: "",
      city: "New York",
      name: "Alex Morgan",
      email: "alex@gather.example",
      line: "12 Garden Street",
      postal: cart.postal,
      phone: "+12125550123",
      instructions: "",
    },
    quote: {
      menuVersionId: "menu_version_1",
      subtotal: 4500,
      premium: 0,
      shipping: 500,
      total: 5000,
      currency: markets.us.currency,
      fingerprint: "quote-fingerprint",
      expiresAt: cutoff,
    },
    recipes: [],
    cutoff,
    policyVersion: "1",
    consentAt: new Date().toISOString(),
    ...over,
  };
}

function placed(db: Db, cart: Cart, over: Record<string, unknown> = {}) {
  return db.table("meal_orders").seed({
    owner_id: owner,
    market: cart.market,
    status: "pending_payment",
    payment: "processing",
    fulfillment: "unallocated",
    total: 5000,
    refunded: 0,
    snapshot: snapshot(cart),
    timeline: [],
    ...over,
  });
}
const orderRow = (db: Db) => db.table("meal_orders").rows[0];
const attemptRows = (db: Db) => db.table("meal_checkout_attempts").rows;

describe("stock held for a box", () => {
  test("a reservation takes one delivery slot and the box's servings from each meal, and releasing returns exactly that", async () => {
    const db = memoryTx();
    const cart = box({ servings: 4 });
    stocked(db, cart, 10);
    await adjustStock(db.tx, cart, "reserve");
    expect(held(db, `us:${date}:lemon-chicken`)).toBe(6);
    expect(held(db, `us:${date}:pesto-pasta`)).toBe(6);
    expect(held(db, `us:${date}:delivery`)).toBe(9);
    await adjustStock(db.tx, cart, "release");
    expect(
      [...cart.recipeIds, "delivery"].map((id) => held(db, `us:${date}:${id}`)),
    ).toEqual([10, 10, 10]);
  });

  test("a meal with no stock row at all, too few servings left, or an unpublished row is sold out", async () => {
    const cart = box();
    const missing = memoryTx();
    stocked(missing, cart);
    missing.table("meal_inventory").rows.splice(1, 1);
    expect(
      await refusal(() => adjustStock(missing.tx, cart, "reserve")),
    ).toMatchObject({ code: "SOLD_OUT", status: 409 });

    const short = memoryTx();
    stocked(short, cart, 1);
    expect(
      await refusal(() => adjustStock(short.tx, cart, "reserve")),
    ).toMatchObject({ code: "SOLD_OUT" });

    const withdrawn = memoryTx();
    stocked(withdrawn, cart, 10);
    withdrawn.table("meal_inventory").rows[0].published = false;
    expect(
      await refusal(() => adjustStock(withdrawn.tx, cart, "reserve")),
    ).toMatchObject({ code: "SOLD_OUT" });
    // The control: the same withdrawn row still accepts a release, because a
    // box already holding stock has to be able to give it back.
    await adjustStock(withdrawn.tx, cart, "release");
    expect(held(withdrawn, `us:${date}:lemon-chicken`)).toBe(12);
  });

  test("a stock row written between the read and the write is a conflict, not a lost update", async () => {
    const db = memoryTx();
    const cart = box();
    stocked(db, cart, 10);
    const table = db.table("meal_inventory");
    const read = table.get.bind(table);
    table.get = async (selector) => {
      const row = await read(selector);
      if (row) table.rows.find((live) => live.id === row.id)!.version += 1;
      return row;
    };
    expect(await refusal(() => adjustStock(db.tx, cart, "reserve"))).toMatchObject({
      code: "CONFLICT",
      status: 409,
    });
    expect(held(db, `us:${date}:lemon-chicken`)).toBe(10);
  });
});

describe("a checkout attempt's hold", () => {
  test("starting an attempt reserves the box, pins the deadline on the order and names the attempt", async () => {
    const db = memoryTx();
    const cart = box();
    stocked(db, cart);
    const order = placed(db, cart, { status: "created", payment: "created" });
    const updated = await startAttempt(db.tx, stored(order), "processing");
    const attempt = attemptRows(db)[0];
    expect(held(db, `us:${date}:delivery`)).toBe(9);
    expect(attempt).toMatchObject({
      order_id: order.id,
      payment: "processing",
      reservation: "active",
      total: 5000,
      currency: markets.us.currency,
    });
    expect(updated).toMatchObject({
      status: "pending_payment",
      payment: "processing",
    });
    const carried = updated.snapshot as OrderSnapshot;
    expect(carried.paymentAttemptId).toBe(attempt.id);
    expect(carried.paymentDeadline).toBe(attempt.expires_at);
    expect(Date.parse(attempt.expires_at as string)).toBeLessThanOrEqual(
      Date.parse(cutoff),
    );
  });

  test("a customer with a payment in flight cannot start a second one, and a settled one is no obstacle", async () => {
    const db = memoryTx();
    db.table("meal_checkout_attempts").seed({
      owner_id: owner,
      market: "us",
      payment: "requires_action",
      reservation: "active",
      expires_at: new Date(Date.now() + 600_000).toISOString(),
    });
    expect(
      await refusal(() => requirePaymentReady(db.tx, owner, "us")),
    ).toMatchObject({ code: "PAYMENT_PENDING", status: 409 });
    // Controls: the same customer in another market, and a customer whose only
    // attempt has a final result, are both free to start.
    await requirePaymentReady(db.tx, owner, "uk");
    attemptRows(db)[0].payment = "succeeded";
    await requirePaymentReady(db.tx, owner, "us");
  });

  // `expireReservations` releases the stock and tells the shopper their meals
  // are no longer reserved, but it leaves `payment` where the provider left it.
  // A block that reads `payment` alone therefore never lifts: the attempt it
  // sends the customer to settle is the one settlement can no longer reach.
  test("an expired hold stops blocking, however its payment was left", async () => {
    for (const [reservation, expires_at] of [
      ["expired", new Date(Date.now() - 600_000).toISOString()],
      ["active", new Date(Date.now() - 1_000).toISOString()],
    ] as const) {
      const db = memoryTx();
      db.table("meal_checkout_attempts").seed({
        owner_id: owner,
        market: "us",
        payment: "requires_action",
        reservation,
        expires_at,
      });
      // The control: the SAME row, live, is refused - so a pass below is the
      // expiry lifting the block and not an empty table.
      attemptRows(db)[0].reservation = "active";
      attemptRows(db)[0].expires_at = new Date(
        Date.now() + 600_000,
      ).toISOString();
      expect(
        await refusal(() => requirePaymentReady(db.tx, owner, "us")),
      ).toMatchObject({ code: "PAYMENT_PENDING", status: 409 });
      attemptRows(db)[0].reservation = reservation;
      attemptRows(db)[0].expires_at = expires_at;
      await requirePaymentReady(db.tx, owner, "us");
    }
  });

  test("expiry releases only overdue active holds in that market and ends the order it still owns", async () => {
    const db = memoryTx();
    const cart = box();
    stocked(db, cart, 10);
    await adjustStock(db.tx, cart, "reserve");
    const now = Date.parse("2026-09-11T12:00:00Z");
    const overdue = placed(db, cart);
    const attempt = db.table("meal_checkout_attempts").seed({
      order_id: overdue.id, owner_id: owner, market: "us",
      payment: "processing", reservation: "active",
      expires_at: new Date(now - 1000).toISOString(),
      cart, total: 5000, currency: markets.us.currency,
    });
    overdue.snapshot = snapshot(cart, { paymentAttemptId: attempt.id });
    // Controls, each differing in one variable: still within its deadline, in
    // another market, and already released.
    const others = ["fresh", "elsewhere", "done"].map((label) =>
      db.table("meal_checkout_attempts").seed({
        order_id: overdue.id, owner_id: "other", market: label === "elsewhere" ? "uk" : "us",
        payment: label === "done" ? "failed" : "processing",
        reservation: label === "done" ? "released" : "active",
        expires_at: new Date(label === "fresh" ? now + 60_000 : now - 1000).toISOString(),
        cart, total: 1, currency: markets.us.currency,
      }),
    );

    expect(await expireReservations(db.tx, "us", now)).toBe(1);
    expect(held(db, `us:${date}:lemon-chicken`)).toBe(10);
    expect(held(db, `us:${date}:delivery`)).toBe(10);
    expect(attemptRows(db).find((row) => row.id === attempt.id)!.reservation).toBe("expired");
    for (const untouched of others)
      expect(
        attemptRows(db).find((row) => row.id === untouched.id)!.reservation,
        String(untouched.id),
      ).toBe(untouched.reservation);
    expect(orderRow(db).status).toBe("checkout_expired");
    expect((orderRow(db).timeline as { key: string }[]).map((entry) => entry.key)).toEqual([
      "checkout_expired",
    ]);
  });

  test("expiry leaves an order whose current attempt is a different one, and one already past payment", async () => {
    const cart = box();
    for (const [label, over] of [
      ["another attempt is current", { snapshot: snapshot(cart, { paymentAttemptId: "other" }) }],
      ["the order already moved on", { status: "confirmed" }],
    ] as const) {
      const db = memoryTx();
      stocked(db, cart, 10);
      await adjustStock(db.tx, cart, "reserve");
      const now = Date.parse("2026-09-11T12:00:00Z");
      const order = placed(db, cart, over);
      const attempt = db.table("meal_checkout_attempts").seed({
        order_id: order.id, owner_id: owner, market: "us",
        payment: "processing", reservation: "active",
        expires_at: new Date(now - 1000).toISOString(),
        cart, total: 5000, currency: markets.us.currency,
      });
      if (!("snapshot" in over))
        order.snapshot = snapshot(cart, { paymentAttemptId: attempt.id });
      expect(await expireReservations(db.tx, "us", now), label).toBe(1);
      // The hold is always released; only the order's own status is guarded.
      expect(attemptRows(db)[0].reservation, label).toBe("expired");
      expect(orderRow(db).status, label).toBe(
        "status" in over ? over.status : "pending_payment",
      );
      expect((orderRow(db).timeline as unknown[]).length, label).toBe(0);
    }
  });

  test("releasing a pending attempt gives the stock back and cancels it, and skips one already final", async () => {
    const cart = box();
    const db = memoryTx();
    stocked(db, cart, 10);
    await adjustStock(db.tx, cart, "reserve");
    const order = placed(db, cart);
    const attempt = db.table("meal_checkout_attempts").seed({
      order_id: order.id, owner_id: owner, market: "us",
      payment: "requires_action", reservation: "active",
      expires_at: new Date(Date.now() + 60_000).toISOString(),
      cart, total: 5000, currency: markets.us.currency,
    });
    order.snapshot = snapshot(cart, { paymentAttemptId: attempt.id });
    await releasePendingAttempt(db.tx, stored(order));
    expect(held(db, `us:${date}:delivery`)).toBe(10);
    expect(attemptRows(db)[0]).toMatchObject({ payment: "canceled", reservation: "released" });

    // Controls: no attempt named at all, and one that already succeeded.
    const bare = memoryTx();
    await releasePendingAttempt(bare.tx, stored(placed(bare, cart)));
    expect(attemptRows(bare)).toEqual([]);

    const settled = memoryTx();
    stocked(settled, cart, 10);
    const paidOrder = placed(settled, cart, { status: "confirmed", payment: "succeeded" });
    const consumed = settled.table("meal_checkout_attempts").seed({
      order_id: paidOrder.id, owner_id: owner, market: "us",
      payment: "succeeded", reservation: "consumed",
      expires_at: new Date(Date.now() + 60_000).toISOString(),
      cart, total: 5000, currency: markets.us.currency,
    });
    paidOrder.snapshot = snapshot(cart, { paymentAttemptId: consumed.id });
    await releasePendingAttempt(settled.tx, stored(paidOrder));
    expect(attemptRows(settled)[0]).toMatchObject({ payment: "succeeded", reservation: "consumed" });
    expect(held(settled, `us:${date}:delivery`)).toBe(10);
  });
});

describe("settling a payment", () => {
  const now = Date.parse("2026-09-11T12:00:00Z");
  async function pending(cart: Cart, over: Record<string, unknown> = {}) {
    const db = memoryTx();
    stocked(db, cart, 10);
    await adjustStock(db.tx, cart, "reserve");
    const order = placed(db, cart, over);
    const attempt = db.table("meal_checkout_attempts").seed({
      order_id: order.id, owner_id: owner, market: "us",
      payment: "processing", reservation: "active",
      expires_at: new Date(now + 60_000).toISOString(),
      cart, total: 5000, currency: markets.us.currency,
    });
    order.snapshot = snapshot(cart, { paymentAttemptId: attempt.id });
    return { db, order, attempt };
  }

  test("a confirmed payment consumes the hold, confirms the order and starts a recurring plan", async () => {
    const { db, attempt } = await pending(box({ recurring: true }));
    const settled = await settleAttempt(db.tx, attempt.id, "succeeded", now);
    expect(settled).toMatchObject({ status: "confirmed", payment: "succeeded" });
    expect(attemptRows(db)[0]).toMatchObject({ payment: "succeeded", reservation: "consumed" });
    expect(held(db, `us:${date}:delivery`)).toBe(9);
    expect((settled.timeline as { key: string }[]).at(-1)!.key).toBe("payment_succeeded");
    expect(db.table("meal_plans").rows[0]).toMatchObject({
      status: "active", owner_id: owner, market: "us",
    });
    // A one-time box leaves no plan behind.
    const single = await pending(box());
    await settleAttempt(single.db.tx, single.attempt.id, "succeeded", now);
    expect(single.db.table("meal_plans").rows).toEqual([]);
  });

  test("repeating the same outcome is idempotent and a different final outcome is refused", async () => {
    const { db, attempt } = await pending(box());
    const first = await settleAttempt(db.tx, attempt.id, "succeeded", now);
    const again = await settleAttempt(db.tx, attempt.id, "succeeded", now);
    expect(again.version).toBe(first.version);
    expect((again.timeline as unknown[]).length).toBe((first.timeline as unknown[]).length);
    expect(await refusal(() => settleAttempt(db.tx, attempt.id, "failed", now))).toMatchObject({
      code: "PAYMENT_FINAL",
      status: 409,
    });
  });

  test("a declined payment releases the hold and returns the order to the customer", async () => {
    const { db, attempt } = await pending(box());
    const settled = await settleAttempt(db.tx, attempt.id, "failed", now);
    expect(settled).toMatchObject({ status: "pending_payment", payment: "failed" });
    expect(attemptRows(db)[0]).toMatchObject({ payment: "failed", reservation: "released" });
    expect(held(db, `us:${date}:delivery`)).toBe(10);
    expect((settled.timeline as { key: string }[]).at(-1)!.key).toBe("payment_failed");
  });

  test("a payment arriving after the hold lapsed enters review instead of confirming", async () => {
    const { db, attempt } = await pending(box({ recurring: true }));
    attemptRows(db)[0].expires_at = new Date(now - 1000).toISOString();
    const settled = await settleAttempt(db.tx, attempt.id, "succeeded", now);
    expect(settled).toMatchObject({ status: "payment_recovery", payment: "succeeded" });
    expect((settled.timeline as { key: string }[]).map((entry) => entry.key)).toEqual([
      "checkout_expired",
      "late_payment",
    ]);
    // The meals went back when the hold lapsed and this payment does not retake them.
    expect(held(db, `us:${date}:delivery`)).toBe(10);
    // A recurring box under review starts no plan.
    expect(db.table("meal_plans").rows).toEqual([]);
  });

  test("an attempt the order no longer names, or whose total or currency moved, needs review", async () => {
    for (const [label, mutate] of [
      ["the order names another attempt", (db: Db) => {
        orderRow(db).snapshot = snapshot(box(), { paymentAttemptId: "other" });
      }],
      ["the total moved", (db: Db) => { orderRow(db).total = 5100; }],
      ["the currency moved", (db: Db) => { attemptRows(db)[0].currency = markets.uk.currency; }],
    ] as const) {
      const { db, attempt } = await pending(box());
      mutate(db);
      expect(
        await refusal(() => settleAttempt(db.tx, attempt.id, "succeeded", now)),
        label,
      ).toMatchObject({ code: "PAYMENT_MISMATCH", status: 409 });
    }
    // The control: untouched, the same call settles.
    const clean = await pending(box());
    expect(await settleAttempt(clean.db.tx, clean.attempt.id, "succeeded", now)).toMatchObject({
      status: "confirmed",
    });
  });

  test("an unknown attempt is not found", async () => {
    const { db } = await pending(box());
    expect(
      await refusal(() => settleAttempt(db.tx, "no-such-attempt", "succeeded", now)),
    ).toMatchObject({ code: "NOT_FOUND", status: 404 });
  });
});

describe("the recurring plan a confirmed box leaves", () => {
  test("the next delivery is a week on, and a second box updates the plan in place rather than adding one", async () => {
    const db = memoryTx();
    const confirmed = { status: "confirmed", payment: "succeeded" };
    const order = placed(db, box({ recurring: true, deliveryDate: "2026-09-15" }), confirmed);
    await activatePlan(db.tx, stored(order));
    expect(db.table("meal_plans").rows).toHaveLength(1);
    expect(db.table("meal_plans").rows[0]).toMatchObject({
      status: "active", next_date: "2026-09-22", skipped: [],
    });
    const later = placed(db, box({ recurring: true, deliveryDate: "2026-09-22" }), confirmed);
    await activatePlan(db.tx, stored(later));
    expect(db.table("meal_plans").rows).toHaveLength(1);
    expect(db.table("meal_plans").rows[0].next_date).toBe("2026-09-29");
    // A one-time box leaves the plan alone.
    await activatePlan(db.tx, stored(placed(db, box(), confirmed)));
    expect(db.table("meal_plans").rows).toHaveLength(1);
  });
});

describe("the order shape the browser receives", () => {
  test("wire carries the stored identity, money and timeline and drops the columns around them", () => {
    const row = {
      id: "ord_1", version: 3, market: "uk", status: "confirmed", payment: "succeeded",
      fulfillment: "packing", total: 5000, refunded: 100,
      snapshot: { cart: box() },
      timeline: [{ at: "2026-09-11T12:00:00.000Z", key: "created", detail: "Box created" }],
      owner_id: owner, created_at: "2026-09-11T11:00:00.000Z",
    };
    const sent = wire(stored(row));
    expect(sent).toEqual({
      id: "ord_1", version: 3, market: "uk", status: "confirmed", payment: "succeeded",
      fulfillment: "packing", total: 5000, refunded: 100,
      snapshot: row.snapshot, timeline: row.timeline,
    });
    expect(Object.keys(sent)).not.toContain("owner_id");
    expect(Object.keys(sent)).not.toContain("created_at");
  });

  test("a timeline entry is stamped now and omits values entirely when none are given", () => {
    const before = Date.now();
    const bare = event("packing", "Your box is being packed.");
    expect(Object.keys(bare).sort()).toEqual(["at", "detail", "key"]);
    expect(Date.parse(bare.at)).toBeGreaterThanOrEqual(before);
    expect(event("refund", "Refunded", { amount: 100 }).values).toEqual({ amount: 100 });
  });
});
