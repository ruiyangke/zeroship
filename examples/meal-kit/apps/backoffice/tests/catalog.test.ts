import { test, expect } from "vitest";
import { defaultCart, quoteCart, assertQuote } from "@gather/meal-kit/domain";
import {
  recipeDraftSchema,
  recipeDraftInputSchema,
  validatePublication,
  recipeText,
} from "@gather/meal-kit/catalog-domain";
import { deliveryDates, cutoffForDate } from "@gather/meal-kit/catalog";
import { recipes, seedRecipeDraft } from "../src/seed-catalog";
import { sampleMenu } from "./fixtures/catalog";
const now = Date.parse("2026-09-11T10:00:00Z");
const cart = {
  ...defaultCart("us"),
  postal: "10001",
  deliveryDate: deliveryDates("us", now)[0],
  recipeIds: recipes.slice(0, 3).map((recipe) => recipe.id),
};

test("published prices and version identities determine a quote", () => {
  const menu = sampleMenu(cart, now);
  const quote = quoteCart(cart, menu, now);
  expect(quote.total).toBe(menu.price * 6 + menu.shipping + 500);
  expect(quote.menuVersionId).toBe(menu.id);
  expect(assertQuote(cart, quote, menu, now).total).toBe(quote.total);
  for (const next of [
    { ...menu, id: "next-published-version" },
    { ...menu, price: menu.price + 1 },
    {
      ...menu,
      recipes: menu.recipes.map((recipe) => ({
        ...recipe,
        versionId: "revised-" + recipe.id,
      })),
    },
  ])
    expect(() => assertQuote(cart, quote, next, now)).toThrow(/updated total/);
  expect(() => quoteCart(cart, { ...menu, market: "cn" }, now)).toThrow(
    /another delivery date/,
  );
});

test("approval requires complete translated content and matched quantities", () => {
  const draft = seedRecipeDraft(recipes[0], (text) => "Translated " + text);
  expect(
    recipeDraftInputSchema.safeParse({
      ...draft,
      translations: { zh: { ...draft.translations.zh, name: "" } },
    }).success,
  ).toBe(true);
  expect(
    recipeDraftSchema.safeParse({
      ...draft,
      translations: { zh: { ...draft.translations.zh, name: "" } },
    }).success,
  ).toBe(false);
  expect(recipeDraftSchema.parse(draft)).toEqual(draft);
  expect(recipeText(draft, "zh").name).toBe("Translated " + draft.name);
  expect(recipeText(draft, "en").name).toBe(draft.name);
  for (const invalid of [
    { ...draft, quantities: [] },
    { ...draft, translations: { zh: { ...draft.translations.zh, steps: [] } } },
    {
      ...draft,
      quantities: draft.quantities.map((value) => ({ ...value, amount: 0 })),
    },
    { ...draft, allergens: ["unknown"] },
    { ...draft, image: "https://untrusted.example/photo.svg" },
  ])
    expect(recipeDraftSchema.safeParse(invalid).success).toBe(false);
});

test("publication checks sale windows, monetary bounds and duplicate versions", () => {
  const draft = {
    price: 1200,
    shipping: 500,
    opensAt: new Date(now).toISOString(),
    closesAt: cutoffForDate("us", cart.deliveryDate),
    offerings: [{ recipeVersionId: "approved", premium: 0 }],
  };
  expect(validatePublication("us", cart.deliveryDate, draft, now)).toEqual(
    draft,
  );
  for (const invalid of [
    { ...draft, closesAt: draft.opensAt },
    {
      ...draft,
      closesAt: new Date(Date.parse(draft.closesAt) + 1).toISOString(),
    },
    { ...draft, price: -1 },
    { ...draft, offerings: [...draft.offerings, ...draft.offerings] },
    { ...draft, offerings: [] },
  ])
    expect(() =>
      validatePublication("us", cart.deliveryDate, invalid, now),
    ).toThrow();
});
