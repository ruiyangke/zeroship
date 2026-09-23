import { z } from "zod";
import {
  markets,
  cutoffForDate,
  deliveryDates,
  type MarketId,
  type Locale,
} from "@gather/meal-kit/catalog";

const text = z.string().trim().max(3000);
export const recipeTextSchema = z.object({
  name: text.max(120),
  subtitle: text.max(240),
  tag: text.max(80),
  equipment: text.max(500),
  ingredients: z.array(text.max(200)).max(40),
  steps: z.array(text).max(30),
  pantry: text.max(500),
  storage: text.max(1000),
  nutritionBasis: text.max(300),
});
export const recipeDraftInputSchema = recipeTextSchema.extend({
  category: z.enum(["classic", "vegetarian", "quick"]),
  minutes: z.number().int().min(1).max(600),
  calories: z.number().int().min(0).max(10000),
  protein: z.number().min(0).max(1000),
  image: z.enum(["chicken", "pasta", "salmon", "bowl", "mushroom", "tofu"]),
  allergens: z
    .array(z.enum(["milk", "wheat", "nuts", "fish", "soy", "sesame"]))
    .max(6),
  baseServings: z.number().int().min(1).max(20),
  quantities: z
    .array(
      z.object({
        amount: z.number().min(0).max(100000),
        unit: z.enum(["g", "ml", "piece"]),
      }),
    )
    .max(40),
  translations: z.object({ zh: recipeTextSchema }),
});
export const recipeDraftSchema = recipeDraftInputSchema.superRefine(
  (recipe, context) => {
    for (const copy of [recipe, recipe.translations.zh]) {
      if (
        [
          copy.name,
          copy.subtitle,
          copy.tag,
          copy.equipment,
          copy.pantry,
          copy.storage,
          copy.nutritionBasis,
        ].some((value) => !value) ||
        !copy.ingredients.length ||
        !copy.steps.length ||
        [...copy.ingredients, ...copy.steps].some((value) => !value)
      )
        context.addIssue({
          code: "custom",
          path: ["translations"],
          message: "Complete all recipe content in both languages.",
        });
    }
    if (recipe.quantities.some((quantity) => quantity.amount <= 0))
      context.addIssue({
        code: "custom",
        path: ["quantities"],
        message: "Enter positive ingredient quantities.",
      });
    if (
      recipe.ingredients.length !== recipe.translations.zh.ingredients.length ||
      recipe.steps.length !== recipe.translations.zh.steps.length ||
      recipe.quantities.length !== recipe.ingredients.length
    )
      context.addIssue({
        code: "custom",
        path: ["ingredients"],
        message: "Match ingredients, quantities and translated steps.",
      });
    if (new Set(recipe.allergens).size !== recipe.allergens.length)
      context.addIssue({
        code: "custom",
        path: ["allergens"],
        message: "Choose each allergen once.",
      });
  },
);
export type RecipeDraft = z.infer<typeof recipeDraftSchema>;
export type Recipe = RecipeDraft & {
  id: string;
  versionId: string;
  revision: number;
  premium: number;
};
export function recipeText(recipe: RecipeDraft, locale: Locale) {
  return locale === "zh" ? recipe.translations.zh : recipe;
}
export const menuDraftSchema = z.object({
  price: z.number().int().min(1).max(1_000_000),
  shipping: z.number().int().min(0).max(1_000_000),
  opensAt: z.string().datetime(),
  closesAt: z.string().datetime(),
  offerings: z
    .array(
      z.object({
        recipeVersionId: z.string().min(1).max(120),
        premium: z.number().int().min(0).max(1_000_000),
      }),
    )
    .max(60),
});
export type MenuDraft = z.infer<typeof menuDraftSchema>;
export type SaleMenu = {
  id: string;
  market: MarketId;
  date: string;
  revision: number;
  price: number;
  shipping: number;
  currency: string;
  opensAt: string;
  closesAt: string;
  recipes: Recipe[];
};
export function validatePublication(
  market: MarketId,
  date: string,
  draft: MenuDraft,
  now = Date.now(),
) {
  const parsed = menuDraftSchema.parse(draft);
  if (!parsed.offerings.length)
    throw Object.assign(
      new Error(/* i18n */ "Add approved recipes before publishing this menu."),
      { status: 400, code: "EMPTY_MENU" },
    );
  if (
    !deliveryDates(market, now).includes(date) ||
    Date.parse(parsed.opensAt) >= Date.parse(parsed.closesAt) ||
    Date.parse(parsed.closesAt) > Date.parse(cutoffForDate(market, date)) ||
    Date.parse(parsed.closesAt) <= now
  )
    throw Object.assign(
      new Error(
        /* i18n */ "Choose a sale window that ends before the delivery cutoff.",
      ),
      { status: 400, code: "INVALID_SALE_WINDOW" },
    );
  if (
    new Set(parsed.offerings.map((o) => o.recipeVersionId)).size !==
    parsed.offerings.length
  )
    throw Object.assign(new Error(/* i18n */ "Choose each recipe only once."), {
      status: 400,
      code: "DUPLICATE_OFFERING",
    });
  return parsed;
}
export function menuIsOpen(menu: SaleMenu, now = Date.now()) {
  return (
    Date.parse(menu.opensAt) <= now &&
    now < Date.parse(menu.closesAt) &&
    menu.currency === markets[menu.market].currency
  );
}
