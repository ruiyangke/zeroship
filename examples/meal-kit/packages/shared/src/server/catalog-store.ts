import type { Id } from "@zeroship/db";
import { markets, type MarketId } from "../catalog";
import { fail } from "../domain";
import {
  menuIsOpen,
  recipeDraftSchema,
  validatePublication,
  type SaleMenu,
  type Recipe,
  type MenuDraft,
} from "../catalog-domain";
import { changed, type Tx } from "./core";

export async function recipeVersion(
  tx: Tx,
  versionId: string,
): Promise<Recipe> {
  const version = await tx.meal_recipe_versions.get(versionId);
  if (!version)
    fail(
      /* i18n */ "Choose an approved recipe version.",
      "RECIPE_NOT_APPROVED",
    );
  const recipe = await tx.meal_recipes.get(version.recipe_id);
  if (!recipe) fail(/* i18n */ "Recipe not found.", "NOT_FOUND", 404);
  return {
    ...recipeDraftSchema.parse(version.content),
    id: recipe.slug,
    versionId: version.id,
    revision: version.revision,
    premium: 0,
  };
}
export async function publishedMenu(
  tx: Tx,
  market: MarketId,
  date: string,
): Promise<SaleMenu | null> {
  const row = await tx.meal_menus.get({ menu_key: `${market}:${date}` });
  if (row?.status !== "published" || !row.published_version_id) return null;
  const version = await tx.meal_menu_versions.get({
    id: row.published_version_id,
    market,
  });
  if (!version || version.menu_id !== row.id)
    throw new Error("Published menu reference is invalid.");
  return { ...(version.content as SaleMenu), id: version.id };
}
export async function sellableMenu(tx: Tx, market: MarketId, date: string) {
  const menu = await publishedMenu(tx, market, date);
  if (!menu || !menuIsOpen(menu))
    fail(
      /* i18n */ "This menu is no longer available. Choose another delivery date.",
      "MENU_UNAVAILABLE",
      409,
    );
  return menu;
}
export async function publishMenuVersion(
  tx: Tx,
  id: string,
  version: number,
  actor: string,
) {
  const row = await tx.meal_menus.get(id);
  if (!row) fail(/* i18n */ "Menu not found.", "NOT_FOUND", 404);
  if (row.version !== version) changed(null);
  const market = row.market as MarketId;
  const draft = validatePublication(
    market,
    row.delivery_date,
    row.draft as MenuDraft,
  );
  const recipes: Recipe[] = [];
  for (const offering of draft.offerings) {
    const recipe = await recipeVersion(tx, offering.recipeVersionId);
    const master = await tx.meal_recipes.get({ slug: recipe.id });
    if (!master || master.archived)
      fail(/* i18n */ "Remove archived recipes before publishing.");
    if (recipes.some((entry) => entry.id === recipe.id))
      fail(/* i18n */ "Choose each recipe only once.");
    recipes.push({ ...recipe, premium: offering.premium });
  }
  const latest = await tx.meal_menu_versions
    .find({ menu_id: row.id as Id<"meal_menus"> })
    .sort({ revision: -1 })
    .limit(1);
  const revision = (latest[0]?.revision ?? 0) + 1;
  const content: Omit<SaleMenu, "id"> = {
    market,
    date: row.delivery_date,
    revision,
    price: draft.price,
    shipping: draft.shipping,
    currency: markets[market].currency,
    opensAt: draft.opensAt,
    closesAt: draft.closesAt,
    recipes,
  };
  const published = await tx.meal_menu_versions.insert({
    menu_id: row.id as Id<"meal_menus">,
    market,
    revision,
    content,
    published_by: actor,
    published_at: new Date().toISOString(),
  });
  changed(
    await tx.meal_menus.update(
      { id: row.id, version },
      {
        published_version_id: published.id,
        status: "published",
        history: [
          ...(row.history as {
            action: string;
            actor: string;
            at: string;
            versionId?: string;
          }[]),
          {
            action: "published",
            actor,
            at: published.published_at,
            versionId: published.id,
          },
        ],
      },
    ),
  );
  for (const recipeId of [...recipes.map((recipe) => recipe.id), "delivery"]) {
    const stock_key = `${market}:${row.delivery_date}:${recipeId}`;
    if (!(await tx.meal_inventory.get({ stock_key })))
      await tx.meal_inventory.insert({
        stock_key,
        market,
        recipe_id: recipeId,
        available: 0,
        published: true,
      });
  }
  return { ...content, id: published.id };
}
