import { z } from "zod";
import { fail, marketSchema, type Order } from "@gather/meal-kit/domain";
import { recipeDraftSchema, type Recipe } from "@gather/meal-kit/catalog-domain";

export const cookingUnitsSchema = z.enum(["metric", "us", "imperial"]);
export type CookingUnits = z.infer<typeof cookingUnitsSchema>;
export type IngredientAmount = Recipe["quantities"][number];
export type DisplayAmount = {
  amount: number;
  unit: "g" | "ml" | "piece" | "oz" | "us_fl_oz" | "uk_fl_oz";
};

export function ingredientAmount(
  quantity: IngredientAmount,
  servings: number,
  baseServings: number,
  units: CookingUnits,
): DisplayAmount {
  const amount = (quantity.amount * servings) / baseServings;
  if (quantity.unit === "piece" || units === "metric")
    return { amount, unit: quantity.unit };
  if (quantity.unit === "g")
    return { amount: amount / 28.349523125, unit: "oz" };
  return units === "us"
    ? { amount: amount / 29.5735295625, unit: "us_fl_oz" }
    : { amount: amount / 28.4130625, unit: "uk_fl_oz" };
}

export function purchasedRecipe(
  order: Pick<Order, "snapshot">,
  recipeId: string,
) {
  const recipe = order.snapshot.recipes.find(
    (recipe) => recipe.id === recipeId,
  );
  if (!recipe)
    fail(/* i18n */ "Recipe not found in this order.", "NOT_FOUND", 404);
  return recipe;
}

export const cookingInputSchema = z.object({
  orderId: z.string().min(1).max(100),
  recipeId: z.string().min(1).max(100),
});
export const feedbackContentSchema = z.object({
  rating: z.number().int().min(1).max(5),
  cookAgain: z.boolean().nullable(),
  comment: z.string().trim().max(2000),
});
export const feedbackSchema = feedbackContentSchema.extend({
  id: z.string(),
  version: z.number().int().positive(),
  createdAt: z.string(),
  updatedAt: z.string(),
});
export const saveFeedbackSchema = cookingInputSchema.extend({
  ...feedbackContentSchema.shape,
  expectedVersion: z.number().int().positive().nullable(),
  requestKey: z.string().uuid(),
});
export type RecipeFeedback = z.infer<typeof feedbackSchema>;
export const cookingRecipeSchema = z.object({
  orderId: z.string(),
  market: marketSchema,
  servings: z.number().int().positive(),
  recipe: recipeDraftSchema.and(
    z.object({
      id: z.string(),
      versionId: z.string(),
      revision: z.number().int().positive(),
      premium: z.number().int(),
    }),
  ),
  canReview: z.boolean(),
  feedback: feedbackSchema.nullable(),
});
export type CookingRecipe = z.infer<typeof cookingRecipeSchema>;
