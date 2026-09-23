import { cartSchema, type Cart } from "@gather/meal-kit/domain";
import {
  draftLifetimeMs,
  draftOrderContents,
  type SavedDraft,
} from "@gather/meal-kit/draft-domain";
import { changed, type Tx } from "@gather/meal-kit/server/core";

export type DraftRow = Awaited<ReturnType<Tx["meal_carts"]["get"]>>;
export function activeDraft(
  row: DraftRow,
  now = Date.now(),
): SavedDraft | null {
  return row && Date.parse(row.expires_at) > now
    ? { id: row.id, version: row.version, cart: cartSchema.parse(row.cart) }
    : null;
}
export async function writeDraft(
  tx: Tx,
  row: DraftRow,
  principal: string,
  cart: Cart,
  requestKey: string,
) {
  const values = {
    cart: cartSchema.parse(cart),
    expires_at: new Date(Date.now() + draftLifetimeMs).toISOString(),
    last_request_key: requestKey,
  };
  return row
    ? changed(
        await tx.meal_carts.update(
          { id: row.id, version: row.version },
          values,
        ),
      )
    : tx.meal_carts.insert({ principal, market: cart.market, ...values });
}

export async function consumeDraft(tx: Tx, owner: string, cart: Cart) {
  const row = await tx.meal_carts.get({
    principal: `user:${owner}`,
    market: cart.market,
  });
  const draft = activeDraft(row);
  if (draft && draftOrderContents(draft.cart) === draftOrderContents(cart))
    await writeDraft(
      tx,
      row,
      `user:${owner}`,
      { ...draft.cart, recipeIds: [] },
      `checkout:${crypto.randomUUID()}`,
    );
}
