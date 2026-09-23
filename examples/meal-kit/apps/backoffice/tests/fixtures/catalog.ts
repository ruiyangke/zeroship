import { recipesForMarket, seedRecipeDraft } from "../../src/seed-catalog";
import { markets, cutoffForDate } from "@gather/meal-kit/catalog";
import type { Cart } from "@gather/meal-kit/domain";
import type { SaleMenu } from "@gather/meal-kit/catalog-domain";
export function sampleMenu(cart: Cart, now: number): SaleMenu {
  return {
    id: `sample-${cart.market}:${cart.deliveryDate}`,
    market: cart.market,
    date: cart.deliveryDate,
    revision: 1,
    price: markets[cart.market].price,
    shipping: markets[cart.market].shipping,
    currency: markets[cart.market].currency,
    opensAt: new Date(now - 86400000).toISOString(),
    closesAt: cutoffForDate(cart.market, cart.deliveryDate),
    recipes: recipesForMarket(cart.market).map((recipe) => ({
      ...seedRecipeDraft(recipe, (text) => text),
      id: recipe.id,
      versionId: `version-${recipe.id}`,
      revision: 1,
      premium: recipe.premium,
    })),
  };
}
