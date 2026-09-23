import { Card } from "@gather/meal-kit/components/ui/card";
import { msg } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { Link } from "react-router-dom";
import { useGather } from "./state";
import { deliveryLabel, markets } from "@gather/meal-kit/catalog";
import { chinaDistricts } from "@gather/meal-kit/countries";
import { reviewBox } from "@gather/meal-kit/box-domain";
import { BoxSummary } from "./components/box-summary";
import { PurchaseSteps } from "./components/purchase-steps";
import {
  CtaLink,
  ErrorState,
  Loading,
  SectionTitle,
} from "@gather/meal-kit/components/shared";

export function BoxPage() {
  const { cart, market, locale, path, catalog, catalogError, refreshCatalog } =
    useGather();
  const { _: t } = useLingui();
  if (catalogError)
    return <ErrorState error={catalogError} retry={refreshCatalog} />;
  if (!catalog) return <Loading />;
  const status = reviewBox(cart, catalog);
  const needsDelivery =
    !status.areaAvailable || !status.dateAvailable || !status.menuAvailable;
  return (
    <section className="section">
      <PurchaseSteps current="box" />
      <SectionTitle
        title={t(msg`Review your box`)}
        body={t(msg`Your meals, your delivery, all in one place.`)}
      />
      <div className="checkout-grid box-review">
        <BoxSummary review />
        <Card className="panel">
          <h2 className="text-2xl mb-5">{t(msg`Delivery details`)}</h2>
          <p>{t(markets[market].name)}</p>
          <p className="mt-2 text-muted-foreground">
            {status.areaAvailable
              ? market === "cn"
                ? `${t(markets.cn.region)} · ${t(chinaDistricts[cart.area.district as keyof typeof chinaDistricts])}`
                : cart.postal
              : t(msg`Add your delivery area`)}
          </p>
          <p className="mt-5">
            {deliveryLabel(cart.deliveryDate, market, locale)}
          </p>
          <p className="mt-2 text-sm">
            {cart.recurring ? t(msg`Weekly plan`) : t(msg`One-time box`)}
          </p>
          <Link
            className="inline-block underline mt-5 text-sm"
            to={path("/plans?step=box")}
          >
            {t(msg`Change delivery or box size`)}
          </Link>
          {!status.dateAvailable || !status.menuAvailable ? (
            <p role="status" className="notice mt-5">
              {t(
                msg`This delivery date is no longer available. Choose a new date, then review your meals.`,
              )}
            </p>
          ) : !status.mealsReady && cart.recipeIds.length > 0 ? (
            <p role="status" className="notice mt-5">
              {t(
                msg`Check the meals marked in your box and finish your selection before checkout.`,
              )}
            </p>
          ) : null}
          <CtaLink
            className="mt-6 w-full"
            to={path(
              needsDelivery
                ? "/plans"
                : status.mealsReady
                  ? "/checkout"
                  : "/menu#choose-meals",
            )}
          >
            {needsDelivery
              ? t(msg`Choose delivery details`)
              : status.mealsReady
                ? t(msg`Continue to checkout`)
                : t(msg`Continue choosing meals`)}
          </CtaLink>
          <Link
            to={path("/menu")}
            className="block text-center underline mt-4 text-sm"
          >
            {t(msg`Back to my meals`)}
          </Link>
        </Card>
      </div>
    </section>
  );
}
