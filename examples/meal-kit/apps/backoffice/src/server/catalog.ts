"use server";
import type { Id } from "@zeroship/db";
import { env } from "zeroship";
import { query, mutation } from "@zeroship/rpc/server";
import { z } from "zod";
import { marketSchema, fail } from "@gather/meal-kit/domain";
import {
  recipeDraftSchema,
  recipeDraftInputSchema,
  menuDraftSchema,
  type RecipeDraft,
} from "@gather/meal-kit/catalog-domain";
import { markets, type MarketId } from "@gather/meal-kit/catalog";
import { must, transact, changed, user } from "@gather/meal-kit/server/core";
import {
  requireRecipeEditor,
  requirePermission,
  staffAccess,
  forbidden,
} from "@gather/meal-kit/server/staff-access";
import { allows } from "@gather/meal-kit/staff-domain";
import { publishMenuVersion } from "@gather/meal-kit/server/catalog-store";

export const getCatalogWorkspace = query(
  async ({ market }: { market: MarketId }) => {
    const access = await staffAccess(user().id);
    const canEditRecipes = Boolean(access?.recipeEditor);
    const canEditMenus = allows(access, "catalog", market);
    if (!canEditRecipes && !canEditMenus) forbidden();
    const recipes = must(await env.db.meal_recipes.find({}).sort({ slug: 1 }));
    const versions = must(
      await env.db.meal_recipe_versions.find({}).sort({ revision: -1 }),
    );
    return {
      canEditRecipes,
      canEditMenus,
      recipes: canEditRecipes
        ? recipes
        : recipes.flatMap((row) => {
            const approved = versions.find(
              (version) => version.recipe_id === row.id,
            );
            return approved ? [{ ...row, draft: approved.content }] : [];
          }),
      versions,
      menus: canEditMenus
        ? must(
            await env.db.meal_menus.find({ market }).sort({ delivery_date: 1 }),
          )
        : [],
    };
  },
  { id: "gather.catalogWorkspace", input: z.object({ market: marketSchema }) },
);

export const saveRecipeDraft = mutation(
  async ({
    id,
    version,
    slug,
    draft,
  }: {
    id?: string;
    version?: number;
    slug: string;
    draft: RecipeDraft;
  }) => {
    return transact(async (tx) => {
      await requireRecipeEditor(tx);
      if (id) {
        const row = await tx.meal_recipes.get(id);
        if (!row || row.slug !== slug)
          fail(/* i18n */ "Recipe not found.", "NOT_FOUND", 404);
        if (row.archived)
          fail(/* i18n */ "Restore this recipe before editing it.");
        return changed(
          await tx.meal_recipes.update({ id, version }, { draft }),
        );
      }
      if (await tx.meal_recipes.get({ slug }))
        fail(
          /* i18n */ "This recipe link is already in use. Choose another.",
          "SLUG_IN_USE",
          409,
        );
      return tx.meal_recipes.insert({ slug, draft, archived: false });
    });
  },
  {
    id: "gather.saveRecipeDraft",
    input: z
      .object({
        id: z.string().optional(),
        version: z.number().int().positive().optional(),
        slug: z
          .string()
          .regex(/^[a-z0-9]+(?:-[a-z0-9]+)*$/)
          .max(100),
        draft: recipeDraftInputSchema,
      })
      .refine((v) => Boolean(v.id) === (v.version !== undefined)),
  },
);

export const approveRecipe = mutation(
  async ({
    id,
    version,
    note,
  }: {
    id: string;
    version: number;
    note: string;
  }) => {
    return transact(async (tx) => {
      const actor = (await requireRecipeEditor(tx)).id;
      const row = await tx.meal_recipes.get(id);
      if (!row) fail(/* i18n */ "Recipe not found.", "NOT_FOUND", 404);
      if (row.version !== version) changed(null);
      if (row.archived)
        fail(/* i18n */ "Restore this recipe before approving it.");
      const parsed = recipeDraftSchema.safeParse(row.draft);
      if (!parsed.success)
        fail(
          /* i18n */ "Complete both languages, ingredient quantities and cooking steps before approval.",
          "INCOMPLETE_RECIPE",
        );
      const draft = parsed.data;
      const prior = await tx.meal_recipe_versions
        .find({ recipe_id: row.id as Id<"meal_recipes"> })
        .sort({ revision: -1 })
        .limit(1);
      if (
        prior[0] &&
        JSON.stringify(prior[0].content) === JSON.stringify(draft)
      )
        return prior[0];
      const approved = await tx.meal_recipe_versions.insert({
        recipe_id: row.id as Id<"meal_recipes">,
        revision: (prior[0]?.revision ?? 0) + 1,
        content: draft,
        approved_by: actor,
        approval_note: note,
        approved_at: new Date().toISOString(),
      });
      changed(await tx.meal_recipes.update({ id, version }, { draft }));
      return approved;
    });
  },
  {
    id: "gather.approveRecipe",
    input: z.object({
      id: z.string(),
      version: z.number().int().positive(),
      note: z.string().trim().min(5).max(1000),
    }),
  },
);

export const archiveRecipe = mutation(
  async ({
    id,
    version,
    archived,
  }: {
    id: string;
    version: number;
    archived: boolean;
  }) => {
    return transact(async (tx) => {
      await requireRecipeEditor(tx);
      return changed(
        await tx.meal_recipes.update({ id, version }, { archived }),
      );
    });
  },
  {
    id: "gather.archiveRecipe",
    input: z.object({
      id: z.string(),
      version: z.number().int().positive(),
      archived: z.boolean(),
    }),
  },
);

export const saveMenuDraft = mutation(
  async ({
    market,
    date,
    id,
    version,
    draft,
  }: {
    market: MarketId;
    date: string;
    id?: string;
    version?: number;
    draft: z.infer<typeof menuDraftSchema>;
  }) => {
    return transact(async (tx) => {
      await requirePermission("catalog", market, tx);
      const row = await tx.meal_menus.get({ menu_key: `${market}:${date}` });
      if (row) {
        if (row.id !== id || row.version !== version) changed(null);
        return changed(
          await tx.meal_menus.update({ id: row.id, version }, { draft }),
        );
      }
      if (id) fail(/* i18n */ "Menu not found.", "NOT_FOUND", 404);
      return tx.meal_menus.insert({
        menu_key: `${market}:${date}`,
        market,
        delivery_date: date,
        draft,
        history: [],
      });
    });
  },
  {
    id: "gather.saveMenuDraft",
    input: z
      .object({
        market: marketSchema,
        date: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
        id: z.string().optional(),
        version: z.number().int().positive().optional(),
        draft: menuDraftSchema,
      })
      .refine((v) => Boolean(v.id) === (v.version !== undefined)),
  },
);

export const publishMenu = mutation(
  async ({ id, version }: { id: string; version: number }) => {
    return transact(async (tx) => {
      const row = await tx.meal_menus.get(id);
      if (!row) fail(/* i18n */ "Menu not found.", "NOT_FOUND", 404);
      const actor = (
        await requirePermission("catalog", marketSchema.parse(row.market), tx)
      ).id;
      return publishMenuVersion(tx, id, version, actor);
    });
  },
  {
    id: "gather.publishMenu",
    input: z.object({ id: z.string(), version: z.number().int().positive() }),
  },
);

export const withdrawMenu = mutation(
  async ({ id, version }: { id: string; version: number }) => {
    return transact(async (tx) => {
      const row = await tx.meal_menus.get(id);
      if (!row) fail(/* i18n */ "Menu not found.", "NOT_FOUND", 404);
      const actor = (
        await requirePermission("catalog", marketSchema.parse(row.market), tx)
      ).id;
      return changed(
        await tx.meal_menus.update(
          { id, version },
          {
            status: "withdrawn",
            history: [
              ...(row.history as {
                action: string;
                actor: string;
                at: string;
                versionId?: string;
              }[]),
              { action: "withdrawn", actor, at: new Date().toISOString() },
            ],
          },
        ),
      );
    });
  },
  {
    id: "gather.withdrawMenu",
    input: z.object({ id: z.string(), version: z.number().int().positive() }),
  },
);
