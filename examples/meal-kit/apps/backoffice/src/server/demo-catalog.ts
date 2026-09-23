"use server";
import type { Id } from "@zeroship/db";
import { mutation } from "@zeroship/rpc/server";
import { z } from "zod";
import { setupI18n } from "@lingui/core";
import { messages } from "@gather/meal-kit/locales/zh/messages.mjs";
import { recipes, marketOfferings, seedRecipeDraft } from "../seed-catalog";
import {
  markets,
  deliveryDates,
  cutoffForDate,
  type MarketId,
} from "@gather/meal-kit/catalog";
import { marketSchema } from "@gather/meal-kit/domain";
import { recipeDraftSchema } from "@gather/meal-kit/catalog-domain";
import { demo, transact } from "@gather/meal-kit/server/core";
import { requireAdministrator } from "@gather/meal-kit/server/staff-access";
import { publishMenuVersion } from "@gather/meal-kit/server/catalog-store";

export const loadSampleMenus = mutation(
  async ({ market }: { market: MarketId }) => {
    const actor = requireAdministrator().id;
    demo();
    const i18n = setupI18n({ locale: "zh", messages: { zh: messages } });
    return transact(async (tx) => {
      const offerings = [];
      for (const seed of recipes.filter((recipe) =>
        Object.hasOwn(marketOfferings[market], recipe.id),
      )) {
        let recipe = await tx.meal_recipes.get({ slug: seed.id });
        if (!recipe) {
          const draft = recipeDraftSchema.parse(
            seedRecipeDraft(seed, (text) => i18n._(text)),
          );
          recipe = await tx.meal_recipes.insert({
            slug: seed.id,
            draft,
            archived: false,
          });
          await tx.meal_recipe_versions.insert({
            recipe_id: recipe.id as Id<"meal_recipes">,
            revision: 1,
            content: draft,
            approved_by: actor,
            approval_note: "Illustrative sample content for the store preview.",
            approved_at: new Date().toISOString(),
          });
        }
        const approved = await tx.meal_recipe_versions
          .find({ recipe_id: recipe.id as Id<"meal_recipes"> })
          .sort({ revision: 1 })
          .limit(1);
        if (approved[0])
          offerings.push({
            recipeVersionId: approved[0].id,
            premium: marketOfferings[market][seed.id],
          });
      }
      for (const date of deliveryDates(market)) {
        const menu_key = `${market}:${date}`;
        if (await tx.meal_menus.get({ menu_key })) continue;
        const menu = await tx.meal_menus.insert({
          menu_key,
          market,
          delivery_date: date,
          history: [],
          draft: {
            price: markets[market].price,
            shipping: markets[market].shipping,
            opensAt: new Date().toISOString(),
            closesAt: cutoffForDate(market, date),
            offerings,
          },
        });
        const published = await publishMenuVersion(
          tx,
          menu.id,
          menu.version,
          actor,
        );
        for (const id of [
          ...published.recipes.map((recipe) => recipe.id),
          "delivery",
        ]) {
          const stock = await tx.meal_inventory.get({
            stock_key: `${market}:${date}:${id}`,
          });
          if (stock)
            await tx.meal_inventory.update(stock.id, {
              available: id === "delivery" ? 60 : 40,
            });
        }
      }
      return { ready: true };
    });
  },
  { id: "gather.loadSampleMenus", input: z.object({ market: marketSchema }) },
);
