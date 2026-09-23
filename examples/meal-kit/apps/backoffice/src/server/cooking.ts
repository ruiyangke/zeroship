"use server";

import { query } from "@zeroship/rpc/server";
import { z } from "zod";
import { transact, changed } from "@gather/meal-kit/server/core";
import { requirePermission } from "@gather/meal-kit/server/staff-access";
import { wire } from "@gather/meal-kit/server/orders";
import { feedbackDto } from "@gather/meal-kit/server/feedback";
import { marketSchema } from "@gather/meal-kit/domain";
import {
  feedbackSchema,
  purchasedRecipe,
} from "@gather/meal-kit/cooking-domain";

// Reviews customers write in the storefront, read here from the same table.
const feedbackListSchema = z.array(
  z.object({
    ...feedbackSchema.shape,
    orderId: z.string(),
    recipeId: z.string(),
    recipeVersionId: z.string(),
    recipeName: z.object({ en: z.string(), zh: z.string() }),
  }),
);
export const getRecipeFeedback = query(
  async ({
    market,
  }: {
    market: z.infer<typeof marketSchema>;
  }): Promise<z.infer<typeof feedbackListSchema>> => {
    return transact(async (tx) => {
      await requirePermission("feedback", market, tx);
      const rows = await tx.meal_recipe_feedback
        .find({ market })
        .sort({ updated_at: -1 })
        .limit(100);
      return Promise.all(
        rows.map(async (feedback) => {
          const order = changed(
            await tx.meal_orders.get({ id: feedback.order_id, market }),
          );
          const recipe = purchasedRecipe(wire(order), feedback.recipe_id);
          return {
            ...feedbackDto(feedback),
            orderId: order.id,
            recipeId: recipe.id,
            recipeVersionId: feedback.recipe_version_id,
            recipeName: { en: recipe.name, zh: recipe.translations.zh.name },
          };
        }),
      );
    });
  },
  {
    id: "gather.recipeFeedback",
    input: z.object({ market: marketSchema }),
    output: feedbackListSchema,
  },
);
