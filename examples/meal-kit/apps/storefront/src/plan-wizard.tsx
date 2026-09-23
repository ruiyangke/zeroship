import {
  Collapsible,
  CollapsibleTrigger,
  CollapsibleContent,
} from "@gather/meal-kit/components/ui/collapsible";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import { ServingSlider } from "./components/serving-slider";
import { DeliveryCalendar } from "./components/delivery-calendar";
import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { ArrowLeft, ArrowRight, MapPin, Check } from "lucide-react";
import { useGather } from "./state";
import { deliveryDates, deliveryLabel, markets } from "@gather/meal-kit/catalog";
import { deliveryEligible, countryPolicies, chinaDistricts } from "@gather/meal-kit/countries";
import type { Cart } from "@gather/meal-kit/domain";
import * as api from "./api";
import { Button, ErrorState, Field, Input, Loading } from "@gather/meal-kit/components/shared";
import { DeliveryAreaFields } from "@gather/meal-kit/components/delivery-area";
import { BoxSummary } from "./components/box-summary";
import { ChoiceGroup } from "@gather/meal-kit/components/choice-group";
import { PurchaseSteps } from "./components/purchase-steps";
import { ContentTransition } from "@gather/meal-kit/components/content-transition";

export function Plans() {
  const {
    cart,
    setCart,
    market,
    locale,
    path,
    catalog,
    catalogError,
    refreshCatalog,
    act,
    busy,
  } = useGather();
  const { _: t } = useLingui();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const current =
    params.get("step") === "box" && deliveryEligible(market, cart)
      ? "plan"
      : "delivery";
  const previous = useRef(current);
  const heading = useRef<HTMLHeadingElement>(null);
  const [outside, setOutside] = useState(false);
  const [email, setEmail] = useState("");
  const [consent, setConsent] = useState(false);
  const [joined, setJoined] = useState(false);
  const dates = deliveryDates(market);
  const knownDates = useRef<string[]>([]);
  if (catalog) knownDates.current = catalog.dates;
  const dateReady = dates.includes(cart.deliveryDate);
  useEffect(() => {
    if (previous.current !== current) {
      heading.current?.focus({ preventScroll: true });
      window.scrollTo({ top: 0, left: 0, behavior: "instant" });
    }
    previous.current = current;
  }, [current]);
  return (
    <section className="section plan-wizard">
      <PurchaseSteps current={current} />
      <ContentTransition
        change={current}
        kind="wizard"
        direction={current === "plan" ? 1 : -1}
      >
        <div className="wizard-heading">
          <p className="eyebrow">{t(msg`MAKE IT YOURS`)}</p>
          <h1 ref={heading} tabIndex={-1}>
            {current === "delivery"
              ? t(msg`Where should we deliver?`)
              : t(msg`Choose your box.`)}
          </h1>
          <p>
            {current === "delivery"
              ? t(
                  msg`Let's check delivery to your area. Your full address comes later.`,
                )
              : t(
                  msg`Choose your portions, delivery day and how often you'd like to order.`,
                )}
          </p>
        </div>
        <div className="wizard-layout">
          <div>
            {current === "delivery" ? (
              <>
                <form
                  className="panel wizard-form"
                  onSubmit={(event) => {
                    event.preventDefault();
                    if (!deliveryEligible(market, cart)) {
                      setOutside(true);
                      return;
                    }
                    navigate(path("/plans?step=box"));
                  }}
                >
                  <div className="wizard-market">
                    <MapPin size={20} />
                    <span>{t(markets[market].name)}</span>
                    <span className="text-sm text-muted-foreground">
                      {t(msg`Delivery country`)}
                    </span>
                  </div>
                  {market === "cn" ? (
                    <DeliveryAreaFields
                      value={cart.area}
                      onChange={(area) => {
                        setCart((c) => ({ ...c, area, postal: "" }));
                        setOutside(false);
                        setJoined(false);
                      }}
                    />
                  ) : (
                    <Field
                      label={t(countryPolicies[market].postalLabel)}
                      hint={t(
                        msg`We'll show meals available for your delivery area.`,
                      )}
                    >
                      <Input
                        required
                        value={cart.postal}
                        autoComplete="postal-code"
                        inputMode={market === "us" ? "numeric" : "text"}
                        autoCapitalize="characters"
                        spellCheck={false}
                        maxLength={20}
                        placeholder={markets[market].postal}
                        onChange={(event) => {
                          setCart((c) => ({
                            ...c,
                            postal: event.target.value,
                          }));
                          setOutside(false);
                          setJoined(false);
                        }}
                      />
                    </Field>
                  )}
                  <Collapsible className="text-sm text-muted-foreground mb-7">
                    <CollapsibleTrigger
                      render={<Button variant="ghost" className="px-0" />}
                    >
                      {t(msg`View delivery areas`)}
                    </CollapsibleTrigger>
                    <CollapsibleContent>
                      <p className="mt-3">
                        {t(markets[market].region)}
                        {market === "cn"
                          ? ` · ${Object.values(chinaDistricts)
                              .map((name) => t(name))
                              .join(" · ")}`
                          : ""}
                      </p>
                    </CollapsibleContent>
                  </Collapsible>
                  {outside && (
                    <p role="alert" className="notice">
                      {t(
                        msg`We're not delivering here yet. You can try another area or ask us to let you know when we arrive.`,
                      )}
                    </p>
                  )}
                  <Button type="submit" size="lg" className="w-full">
                    {t(msg`Continue to box size`)}
                    <ArrowRight />
                  </Button>
                </form>
                {outside && (
                  <div className="panel mt-5">
                    {joined ? (
                      <div role="status">
                        <Check className="mb-3" />
                        <h2 className="text-2xl">
                          {t(msg`You're on the list.`)}
                        </h2>
                        <p className="mt-3">
                          {t(
                            msg`We'll email you when delivery opens in your area.`,
                          )}
                        </p>
                      </div>
                    ) : (
                      <form
                        onSubmit={(event) => {
                          event.preventDefault();
                          void act(async () => {
                            await api.joinWaitlist({
                              email,
                              market,
                              postal: cart.postal,
                              area: cart.area,
                              consent: true,
                            });
                            setJoined(true);
                          });
                        }}
                      >
                        <h2 className="text-2xl mb-4">
                          {t(msg`Be the first to know`)}
                        </h2>
                        <Field label={t(msg`Email`)}>
                          <Input
                            type="email"
                            autoComplete="email"
                            required
                            value={email}
                            onChange={(event) => setEmail(event.target.value)}
                          />
                        </Field>
                        <Label className="check-row">
                          <Checkbox
                            required
                            checked={consent}
                            onCheckedChange={(event) => setConsent(event)}
                          />
                          <span>
                            {t(
                              msg`Email me when delivery is available in my area.`,
                            )}
                          </span>
                        </Label>
                        <Button type="submit" disabled={!consent || busy}>
                          {busy ? t(msg`Saving…`) : t(msg`Notify me`)}
                        </Button>
                      </form>
                    )}
                  </div>
                )}
              </>
            ) : (
              <form
                className="panel wizard-form"
                onSubmit={(event) => {
                  event.preventDefault();
                  if (
                    dateReady &&
                    catalog?.menu &&
                    catalog.dates.includes(cart.deliveryDate)
                  )
                    navigate(path("/menu"));
                }}
              >
                <ServingSlider
                  value={cart.servings}
                  onChange={(servings) => setCart((c) => ({ ...c, servings }))}
                />
                <ChoiceGroup<Cart["mealCount"]>
                  columns
                  label={t(msg`Meals per box`)}
                  value={cart.mealCount}
                  onChange={(mealCount) =>
                    setCart((c) => ({ ...c, mealCount }))
                  }
                  options={[
                    { value: 2, label: t(msg`2 meals`) },
                    { value: 3, label: t(msg`3 meals`) },
                    { value: 4, label: t(msg`4 meals`) },
                  ]}
                />
                {cart.recipeIds.length > cart.mealCount && (
                  <p role="status" className="notice mb-6">
                    {t(
                      msg`Your meals are still selected. Review your box to choose which ones to keep.`,
                    )}
                    <Link className="block underline mt-2" to={path("/box")}>
                      {t(msg`Review your box`)}
                    </Link>
                  </p>
                )}
                <ChoiceGroup
                  label={t(msg`Delivery frequency`)}
                  value={cart.recurring ? "weekly" : "once"}
                  onChange={(value) =>
                    setCart((c) => ({ ...c, recurring: value === "weekly" }))
                  }
                  options={[
                    {
                      value: "once",
                      label: t(msg`One-time box`),
                      description: t(msg`Try a box without a weekly plan.`),
                    },
                    {
                      value: "weekly",
                      label: t(msg`Weekly plan`),
                      description: t(
                        msg`Save your choices for each week. Review and confirm each box before ordering.`,
                      ),
                    },
                  ]}
                />
                <DeliveryCalendar
                  value={cart.deliveryDate}
                  dates={knownDates.current}
                  onChange={(deliveryDate) =>
                    setCart((c) => ({ ...c, deliveryDate }))
                  }
                />
                {!dateReady && (
                  <p role="alert" className="notice">
                    {t(
                      msg`Your previous delivery date is no longer available. Choose a new date to continue.`,
                    )}
                  </p>
                )}
                {catalogError ? (
                  <ErrorState error={catalogError} retry={refreshCatalog} />
                ) : !catalog ? (
                  <Loading />
                ) : !catalog.menu && dateReady ? (
                  <p role="status" className="notice">
                    {t(
                      msg`No menu is available for this date. Please choose another delivery day.`,
                    )}
                  </p>
                ) : null}
                <div className="wizard-mobile-summary">
                  <BoxSummary planning />
                </div>
                <div className="wizard-actions">
                  <Link to={path("/plans")}>
                    <ArrowLeft size={16} />
                    {t(msg`Back`)}
                  </Link>
                  <Button
                    size="lg"
                    type="submit"
                    disabled={
                      !dateReady ||
                      !catalog?.menu ||
                      !catalog.dates.includes(cart.deliveryDate) ||
                      !!catalogError
                    }
                  >
                    {t(msg`See available meals`)}
                    <ArrowRight />
                  </Button>
                </div>
              </form>
            )}
          </div>
          {current === "plan" ? (
            <div className="wizard-sidebar">
              <div className="wizard-destination">
                <MapPin size={18} />
                <div>
                  <strong>{t(markets[market].name)}</strong>
                  <p>
                    {market === "cn"
                      ? `${t(markets.cn.region)} · ${t(chinaDistricts[cart.area.district as keyof typeof chinaDistricts])}`
                      : cart.postal}
                  </p>
                </div>
                <Link to={path("/plans")}>{t(msg`Change`)}</Link>
              </div>
              <BoxSummary planning />
            </div>
          ) : (
            <div className="wizard-intro">
              <img src="/media/hero.png" alt="" />
              <div>
                <h2>{t(msg`Good dinners start here.`)}</h2>
                <p>
                  {t(
                    msg`First, find your delivery options. Then choose the meals you look forward to cooking.`,
                  )}
                </p>
              </div>
            </div>
          )}
        </div>
      </ContentTransition>
    </section>
  );
}
