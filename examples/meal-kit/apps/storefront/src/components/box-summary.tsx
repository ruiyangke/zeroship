import { Card } from "@gather/meal-kit/components/ui/card";
import { boxSizeMessage } from "@gather/meal-kit/box-copy";
import { msg, plural } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { Link } from "react-router-dom";
import { Minus } from "lucide-react";
import { useGather } from "../state";
import { recipeText } from "@gather/meal-kit/catalog-domain";
import { money } from "@gather/meal-kit/catalog";
import type { Quote } from "@gather/meal-kit/domain";
import { reviewBox } from "@gather/meal-kit/box-domain";
import { Button, CtaLink, ErrorState, Loading } from "@gather/meal-kit/components/shared";

export function BoxSummary({
  planning = false,
  checkout = false,
  review = false,
  quote,
}: {
  planning?: boolean;
  checkout?: boolean;
  review?: boolean;
  quote?: Quote;
}) {
  const {
    cart,
    setCart,
    locale,
    market,
    path,
    catalog,
    catalogError,
    refreshCatalog,
  } = useGather();
  const { _: t } = useLingui();
  if (catalogError)
    return <ErrorState error={catalogError} retry={refreshCatalog} />;
  if (!catalog) return <Loading />;
  const status = reviewBox(cart, catalog);
  const menu = catalog.menu;
  const subtotal =
    quote?.subtotal ?? (menu?.price ?? 0) * cart.servings * cart.mealCount;
  const premium =
    quote?.premium ??
    status.lines.reduce(
      (sum, line) => sum + (line.recipe?.premium ?? 0) * cart.servings,
      0,
    );
  const shipping = quote?.shipping ?? menu?.shipping ?? 0;
  const showPrice = checkout ? !!quote : status.menuAvailable;
  return (
    <Card className="box-summary gap-0 ring-0">
      <h2>{checkout ? t(msg`Order summary`) : t(msg`Your box`)}</h2>
      <p className="text-sm text-muted-foreground">
        {t(boxSizeMessage(cart.mealCount, cart.servings))}
      </p>
      {!planning && (
        <div className="box-slots">
          {status.lines.map(({ id, recipe, problem }, index) => (
            <div className="box-slot filled" key={`${id}:${index}`}>
              {recipe && <img src={`/media/${recipe.image}.png`} alt="" />}
              <div className="flex-1 min-w-0">
                {recipe ? (
                  <Link to={path(`/recipes/${id}`)}>
                    {recipeText(recipe, locale).name}
                  </Link>
                ) : (
                  <span>{t(msg`Unavailable meal`)}</span>
                )}
                {problem && (
                  <p className="text-destructive mt-1">{t(problem)}</p>
                )}
              </div>
              {!checkout && (
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label={
                    recipe
                      ? t(msg`Remove ${recipeText(recipe, locale).name}`)
                      : t(msg`Remove unavailable meal`)
                  }
                  onClick={() =>
                    setCart((c) => ({
                      ...c,
                      recipeIds: c.recipeIds.filter((_, i) => i !== index),
                    }))
                  }
                >
                  <Minus size={16} />
                </Button>
              )}
            </div>
          ))}
          {Array.from(
            { length: Math.max(0, cart.mealCount - cart.recipeIds.length) },
            (_, i) => (
              <Link
                className="box-slot"
                key={`empty:${i}`}
                to={path("/menu#choose-meals")}
              >
                <span className="step-number">
                  {cart.recipeIds.length + i + 1}
                </span>
                {t(msg`Choose a meal`)}
              </Link>
            ),
          )}
        </div>
      )}
      {cart.recipeIds.length > cart.mealCount && (
        <p role="status" className="notice">
          {t(
            msg`Your box size changed. Remove a meal or choose a larger box to keep all your selections.`,
          )}
        </p>
      )}
      {showPrice ? (
        <>
          <div className="summary-line">
            <span>{t(msg`Meals`)}</span>
            <span>{money(subtotal, market, locale)}</span>
          </div>
          {premium > 0 && (
            <div className="summary-line">
              <span>{t(msg`Premium selections`)}</span>
              <span>{money(premium, market, locale)}</span>
            </div>
          )}
          <div className="summary-line">
            <span>{t(msg`Delivery fee`)}</span>
            <span>{money(shipping, market, locale)}</span>
          </div>
          <div className="summary-line summary-total">
            <span>
              {checkout ? t(msg`Total due`) : t(msg`Estimated total`)}
            </span>
            <span>
              {money(
                quote?.total ?? subtotal + premium + shipping,
                market,
                locale,
              )}
            </span>
          </div>
          {!checkout && (
            <p className="my-3 text-xs text-muted-foreground">
              {t(
                msg`Based on your box size and selected extras. Confirm the final total at checkout.`,
              )}
            </p>
          )}
        </>
      ) : (
        <p className="notice">
          {checkout
            ? t(msg`Your total will appear when it is ready.`)
            : t(msg`Choose an available delivery date to see prices.`)}
        </p>
      )}
      {!checkout && !planning && !review && (
        <>
          {cart.recipeIds.length < cart.mealCount && (
            <p className="text-sm my-4" role="status">
              {t(
                msg({
                  message: plural(cart.mealCount - cart.recipeIds.length, {
                    one: "Choose # more meal",
                    other: "Choose # more meals",
                  }),
                }),
              )}
            </p>
          )}
          <CtaLink to={path("/box")} className="w-full">
            {t(msg`Review your box`)}
          </CtaLink>
        </>
      )}
      <p className="mt-4 text-xs text-muted-foreground">
        {cart.recurring
          ? t(msg`Review and confirm each future box in My deliveries.`)
          : t(msg`One-time purchase. No recurring deliveries.`)}
      </p>
    </Card>
  );
}
