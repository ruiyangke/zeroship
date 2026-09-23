import type { Cart } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";
import { menuIsOpen, type SaleMenu } from "@gather/meal-kit/catalog-domain";
import { deliveryEligible } from "@gather/meal-kit/countries";

export type BoxCatalog = {
  menu: SaleMenu | null;
  availability: { recipeId: string; published: boolean; available: number }[];
};

export function reviewBox(cart: Cart, catalog: BoxCatalog, now = Date.now()) {
  const menu = catalog.menu;
  const dateAvailable = deliveryDates(cart.market, now).includes(
    cart.deliveryDate,
  );
  const menuAvailable =
    !!menu &&
    menu.market === cart.market &&
    menu.date === cart.deliveryDate &&
    menuIsOpen(menu, now);
  const lines = cart.recipeIds.map((id) => {
    const recipe = menu?.recipes.find((r) => r.id === id);
    const stock = catalog.availability.find((r) => r.recipeId === id);
    const problem = !recipe
      ? /* i18n */ "No longer on this menu"
      : recipe.allergens.some((a) => cart.exclude.includes(a))
        ? /* i18n */ "Contains an ingredient you excluded"
        : !stock?.published || stock.available < cart.servings
          ? /* i18n */ "Sold out for this delivery"
          : null;
    return { id, recipe, problem };
  });
  const countMatches =
    cart.recipeIds.length === cart.mealCount &&
    new Set(cart.recipeIds).size === cart.recipeIds.length;
  const areaAvailable = deliveryEligible(cart.market, cart);
  const mealsReady = countMatches && lines.every((line) => !line.problem);
  return {
    lines,
    dateAvailable,
    menuAvailable,
    areaAvailable,
    mealsReady,
    ready: dateAvailable && menuAvailable && areaAvailable && mealsReady,
  };
}
