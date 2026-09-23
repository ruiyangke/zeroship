import { z } from "zod";
import { cartSchema, defaultCart, type Cart } from "@gather/meal-kit/domain";

export const draftLifetimeMs = 30 * 24 * 60 * 60_000;
export const draftRevisionSchema = z.object({
  id: z.string().min(1).max(100),
  version: z.number().int().positive(),
});
export const savedDraftSchema = draftRevisionSchema.extend({
  cart: cartSchema,
});
export const draftLoadSchema = z.object({
  draft: savedDraftSchema.nullable(),
  guest: savedDraftSchema.nullable(),
});
export const draftSaveSchema = z.object({
  saved: z.boolean(),
  state: draftLoadSchema,
});
export type SavedDraft = z.infer<typeof savedDraftSchema>;
export type DraftRevision = z.infer<typeof draftRevisionSchema>;
export type DraftLoad = z.infer<typeof draftLoadSchema>;
export type DraftSave = z.infer<typeof draftSaveSchema>;
export const draftRevision = (draft: SavedDraft | null): DraftRevision | null =>
  draft && { id: draft.id, version: draft.version };
export const sameRevision = (
  left: DraftRevision | null,
  right: DraftRevision | null,
) => left?.id === right?.id && left?.version === right?.version;
export const draftContents = (cart: Cart) =>
  JSON.stringify(cartSchema.parse(cart));

export const draftHasChoices = (cart: Cart) =>
  draftContents(cart) !== draftContents(defaultCart(cart.market));

export function draftOrderContents(cart: Cart) {
  const { postal, area, ...selection } = cartSchema.parse(cart);
  return JSON.stringify(selection);
}
