import { env } from "zeroship";
import type { MarketId } from "../catalog";
import {
  fail,
  type Cart,
  type Order,
  type OrderSnapshot,
  type TimelineEvent,
} from "../domain";
import { must, user, type Tx } from "./core";

export function event(
  key: string,
  detail: string,
  values?: TimelineEvent["values"],
): TimelineEvent {
  return {
    at: new Date().toISOString(),
    key,
    detail,
    ...(values && { values }),
  };
}
export type OrderRow = Awaited<
  ReturnType<typeof env.db.meal_orders.get>
>["data"];
export function wire(row: NonNullable<OrderRow>): Order {
  return {
    id: row.id,
    version: row.version,
    market: row.market as MarketId,
    status: row.status,
    payment: row.payment,
    fulfillment: row.fulfillment,
    total: row.total,
    refunded: row.refunded,
    snapshot: row.snapshot as OrderSnapshot,
    timeline: row.timeline as TimelineEvent[],
  };
}
/**
 * The caller's own order, read through `tx` when one is open.
 *
 * A procedure that goes on to WRITE must pass its transaction. The dev tier
 * feeds every connection from one actor thread with a blocking busy handler, so
 * an autocommit statement issued beside an open transaction parks that thread
 * against a lock the transaction itself holds, and the wait can only end when
 * the budget does. Reading here and writing there is exactly that shape.
 */
export async function owned(id: string, tx?: Tx) {
  const row = tx
    ? await tx.meal_orders.get({ id, owner_id: user().id })
    : must(await env.db.meal_orders.get({ id, owner_id: user().id }));
  if (!row) fail(/* i18n */ "Order not found.", "NOT_FOUND", 404);
  return row;
}

export async function adjustStock(
  tx: Tx,
  cart: Cart,
  direction: "reserve" | "release",
) {
  for (const recipeId of [...cart.recipeIds, "delivery"]) {
    const stock_key = `${cart.market}:${cart.deliveryDate}:${recipeId}`;
    let stock = await tx.meal_inventory.get({ stock_key });
    if (!stock)
      fail(
        /* i18n */ "A meal or delivery slot just sold out. Please update your box.",
        "SOLD_OUT",
        409,
      );
    const amount = recipeId === "delivery" ? 1 : cart.servings;
    if (
      direction === "reserve" &&
      (!stock.published || stock.available < amount)
    )
      fail(
        /* i18n */ "A meal or delivery slot just sold out. Please update your box.",
        "SOLD_OUT",
        409,
      );
    const updated = await tx.meal_inventory.update(
      { id: stock.id, version: stock.version },
      {
        available:
          stock.available + (direction === "reserve" ? -amount : amount),
      },
    );
    if (!updated)
      fail(/* i18n */ "Availability changed. Please retry.", "CONFLICT", 409);
  }
}
