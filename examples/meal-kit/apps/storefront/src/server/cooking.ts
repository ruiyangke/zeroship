"use server";

import { env } from "zeroship";
import type { Id } from "@zeroship/db";
import { query, mutation } from "@zeroship/rpc/server";
import { z } from "zod";
import { must, user, transact, changed } from "@gather/meal-kit/server/core";
import { owned, wire } from "@gather/meal-kit/server/orders";
import { feedbackDto } from "@gather/meal-kit/server/feedback";
import { fail } from "@gather/meal-kit/domain";
import {
  cookingInputSchema,
  cookingRecipeSchema,
  feedbackSchema,
  feedbackContentSchema,
  saveFeedbackSchema,
  purchasedRecipe,
} from "@gather/meal-kit/cooking-domain";

export const getCookingRecipe = query(
  async ({ orderId, recipeId }: z.infer<typeof cookingInputSchema>) => {
    const row = await owned(orderId);
    const order = wire(row);
    const recipe = purchasedRecipe(order, recipeId);
    const feedback = must(
      await env.db.meal_recipe_feedback.get({
        owner_id: user().id,
        order_id: orderId as Id<"meal_orders">,
        recipe_version_id: recipe.versionId as Id<"meal_recipe_versions">,
      }),
    );
    return {
      orderId,
      market: order.market,
      servings: order.snapshot.cart.servings,
      recipe,
      canReview: order.fulfillment === "delivered",
      feedback: feedback ? feedbackDto(feedback) : null,
    };
  },
  {
    id: "gather.cookingRecipe",
    input: cookingInputSchema,
    output: cookingRecipeSchema,
  },
);

export const saveRecipeFeedback = mutation(
  async (input: z.infer<typeof saveFeedbackSchema>) => {
    const owner_id = user().id;
    return transact(async (tx) => {
      const row = await tx.meal_orders.get({ id: input.orderId, owner_id });
      if (!row) fail(/* i18n */ "Order not found.", "NOT_FOUND", 404);
      const recipe = purchasedRecipe(wire(row), input.recipeId);
      if (row.fulfillment !== "delivered")
        fail(
          /* i18n */ "You can review this meal after your box is delivered.",
          "NOT_DELIVERED",
          409,
        );
      const existing = await tx.meal_recipe_feedback.get({
        owner_id,
        order_id: row.id as Id<"meal_orders">,
        recipe_version_id: recipe.versionId as Id<"meal_recipe_versions">,
      });
      const content = feedbackContentSchema.parse(input);
      if (existing?.last_request_key === input.requestKey) {
        if (
          JSON.stringify(feedbackContentSchema.parse(feedbackDto(existing))) !==
          JSON.stringify(content)
        )
          fail(
            /* i18n */ "These details have changed. Refresh the page and try again.",
            "KEY_REUSED",
            409,
          );
        return feedbackDto(existing);
      }
      if ((existing?.version ?? null) !== input.expectedVersion)
        fail(
          /* i18n */ "Your feedback changed on another device. Load the saved feedback before editing it.",
          "CONFLICT",
          409,
        );
      const values = {
        rating: content.rating,
        cook_again: content.cookAgain ?? undefined,
        comment: content.comment,
        last_request_key: input.requestKey,
      };
      const saved = existing
        ? changed(
            await tx.meal_recipe_feedback.update(
              { id: existing.id, owner_id, version: input.expectedVersion! },
              values,
            ),
          )
        : await tx.meal_recipe_feedback.insert({
            owner_id,
            order_id: row.id as Id<"meal_orders">,
            market: row.market,
            recipe_id: recipe.id,
            recipe_version_id: recipe.versionId as Id<"meal_recipe_versions">,
            ...values,
          });
      return feedbackDto(saved);
    });
  },
  {
    id: "gather.saveRecipeFeedback",
    input: saveFeedbackSchema,
    output: feedbackSchema,
  },
);

