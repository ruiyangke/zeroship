"use server";
import type { Id } from "@zeroship/db";
import { query } from "@zeroship/rpc/server";
import { z } from "zod";
import { fail } from "@gather/meal-kit/domain";
import { transact } from "@gather/meal-kit/server/core";
import { recipeVersion } from "@gather/meal-kit/server/catalog-store";

// The published recipe a shopper opens. The rows come from the catalog the
// back office authors and approves, in the database both apps bind.
export const getRecipe = query(
  async ({ slug }: { slug: string }) =>
    transact(async (tx) => {
      const row = await tx.meal_recipes.get({ slug, archived: false });
      if (!row) fail(/* i18n */ "Recipe not found.", "NOT_FOUND", 404);
      const versions = await tx.meal_recipe_versions
        .find({ recipe_id: row.id as Id<"meal_recipes"> })
        .sort({ revision: -1 })
        .limit(1);
      if (!versions.length)
        fail(/* i18n */ "Recipe not found.", "NOT_FOUND", 404);
      return recipeVersion(tx, versions[0].id);
    }),
  { id: "gather.recipe", input: z.object({ slug: z.string().max(100) }) },
);
