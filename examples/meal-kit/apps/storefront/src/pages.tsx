import { RecipeCard } from "./components/recipe-card";
import {
  Accordion,
  AccordionItem,
  AccordionTrigger,
  AccordionContent,
} from "@gather/meal-kit/components/ui/accordion";
import { SelectItem } from "@gather/meal-kit/components/select-field";
import { ToggleGroup, ToggleGroupItem } from "@gather/meal-kit/components/ui/toggle-group";

import { FieldSet, FieldLegend } from "@gather/meal-kit/components/ui/field";
import {
  Sheet,
  SheetContent,
  SheetTitle,
  SheetDescription,
  SheetTrigger,
} from "@gather/meal-kit/components/ui/sheet";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import { ServingSlider } from "./components/serving-slider";
import { BoxSummary } from "./components/box-summary";
import { PurchaseSteps } from "./components/purchase-steps";
import { IngredientQuantity } from "./components/ingredient-quantity";
import { StepTimer } from "./components/cooking-timer";
import { RecipeFeedbackForm } from "./components/recipe-feedback";
import { ChoiceGroup } from "@gather/meal-kit/components/choice-group";
import type { CookingRecipe, CookingUnits } from "@gather/meal-kit/cooking-domain";
import { recipeText, type Recipe } from "@gather/meal-kit/catalog-domain";
import { allergenLabels } from "@gather/meal-kit/catalog";
import { useLingui } from "@lingui/react";
import { msg, plural } from "@lingui/core/macro";
import { Trans } from "@lingui/react/macro";
import { useId, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ArrowRight,
  Box,
  ChefHat,
  Check,
  Leaf,
  SlidersHorizontal,
  Truck,
  Utensils,
  Printer,
} from "lucide-react";
import { useGather } from "./state";
import { markets, money, deliveryDates, deliveryLabel } from "@gather/meal-kit/catalog";
import { type Cart } from "@gather/meal-kit/domain";
import * as api from "./api";
import {
  Badge,
  Button,
  CtaLink,
  Empty,
  ErrorState,
  Field,
  Input,
  Loading,
  SectionTitle,
  Select,
  useLoad,
} from "@gather/meal-kit/components/shared";

export function Home() {
  const { path, market, locale, catalog, catalogError, refreshCatalog } =
    useGather();
  const { _: t } = useLingui();
  return (
    <>
      <section className="hero">
        <div className="hero-copy">
          <p className="eyebrow">{t(msg`FRESH INGREDIENTS. EVERYDAY JOY.`)}</p>
          <h1>
            <Trans>
              Make room for
              <br />
              <em>good food.</em>
            </Trans>
          </h1>
          <p>
            {t(
              msg`Thoughtfully chosen recipes. Fresh ingredients at your door. A little less to do, a little more to enjoy.`,
            )}
          </p>
          <div className="mt-8">
            <CtaLink to={path("/plans")}>{t(msg`Find your first box`)}</CtaLink>
          </div>
          <p className="mt-5 !text-[10px]">
            {t(msg`Flexible plans. Skip or pause before your order deadline.`)}
          </p>
        </div>
        <div className="hero-photo">
          <img
            src="/media/hero.png"
            alt={t(
              msg`Freshly prepared chicken, vegetables and colorful bowls on a dinner table`,
            )}
            fetchPriority="high"
          />
          <div className="hero-note">
            <Leaf size={27} strokeWidth={1.3} />
            <div>
              <strong>{t(msg`A fresh start to your week`)}</strong>
              <span>{t(msg`Seasonal inspiration, delivered`)}</span>
            </div>
          </div>
        </div>
      </section>
      <div className="reassurance">
        <span>
          <Leaf size={17} />
          {t(msg`Fresh, thoughtfully sourced`)}
        </span>
        <span>
          <Utensils size={17} />
          {t(msg`Recipes you'll want again`)}
        </span>
        <span>
          <Box size={17} />
          {t(msg`Your week, your way`)}
        </span>
      </div>
      <section className="section">
        <SectionTitle
          eyebrow={t(msg`A TASTE OF WHAT'S COOKING`)}
          title={t(msg`Meet your next favorite.`)}
          body={t(
            msg`Bright flavors, comforting classics, and something a little unexpected.`,
          )}
        >
          <CtaLink outline to={path("/menu")}>
            {t(msg`See the full menu`)}
          </CtaLink>
        </SectionTitle>
        {catalogError ? (
          <ErrorState error={catalogError} retry={refreshCatalog} />
        ) : !catalog ? (
          <Loading />
        ) : !catalog.recipes.length ? (
          <Empty
            title={t(msg`The menu is being prepared`)}
            text={t(
              msg`Choose another delivery date to explore available meals.`,
            )}
          >
            <CtaLink to={path("/plans")}>
              {t(msg`Choose a delivery date`)}
            </CtaLink>
          </Empty>
        ) : (
          <div className="meal-grid">
            {catalog.recipes.slice(0, 3).map((r) => (
              <RecipeCard key={r.id} recipe={r} compact />
            ))}
          </div>
        )}
      </section>
      <section className="how-section">
        <p className="eyebrow">{t(msg`GOOD DINNERS, SIMPLY DONE`)}</p>
        <h2 className="text-4xl">{t(msg`From our kitchen to yours.`)}</h2>
        <div className="how-grid">
          {[
            [
              Utensils,
              t(msg`Make it yours`),
              t(msg`Pick the meals and portions that fit your table.`),
            ],
            [
              Truck,
              t(msg`We'll bring the fresh`),
              t(
                msg`Choose a delivery day. Your ingredients arrive ready to cook.`,
              ),
            ],
            [
              ChefHat,
              t(msg`Cook. Gather. Enjoy.`),
              t(
                msg`Follow a few simple steps and make dinnertime your favorite time.`,
              ),
            ],
          ].map(([Icon, title, body], i) => {
            const I = Icon as typeof Utensils;
            return (
              <div key={i}>
                <div className="how-icon">
                  <I size={24} strokeWidth={1.4} />
                </div>
                <h3>{title as string}</h3>
                <p>{body as string}</p>
              </div>
            );
          })}
        </div>
      </section>
      <section className="newsletter">
        <div>
          <h2>{t(msg`Your table. Your kind of good.`)}</h2>
          <p>
            {catalog?.menu ? (
              <>
                {t(msg`Choose a plan that feels right, starting at`)}{" "}
                {money(catalog.menu.price, market, locale)}{" "}
                {t(msg`per serving, plus delivery.`)}
              </>
            ) : (
              t(msg`Pick the meals and portions that fit your table.`)
            )}
          </p>
        </div>
        <Link
          className="cta !bg-[#eff1df] !text-primary !border-transparent"
          to={path("/plans")}
        >
          {t(msg`Let's get cooking`)}
          <ArrowRight size={17} />
        </Link>
      </section>
    </>
  );
}

export function MenuPage() {
  const {
    cart,
    setCart,
    market,
    locale,
    path,
    catalog,
    catalogError,
    refreshCatalog,
  } = useGather();
  const { _: t } = useLingui();
  const [filter, setFilter] = useState("all");
  const [search, setSearch] = useState("");
  const [showAllergens, setShowAllergens] = useState(false);
  const result = {
    data: catalog,
    error: catalogError,
    refresh: refreshCatalog,
  };
  const shown = (result.data?.recipes ?? []).filter(
    (r) =>
      (filter === "all" || r.category === filter) &&
      !r.allergens.some((a) =>
        cart.exclude.includes(a as Cart["exclude"][number]),
      ) &&
      recipeText(r, locale).name.toLowerCase().includes(search.toLowerCase()),
  );
  return (
    <section className="section">
      <PurchaseSteps current="menu" />
      <SectionTitle
        eyebrow={t(msg`ON THE MENU`)}
        title={t(msg`What sounds good this week?`)}
        body={t(
          msg`A delicious mix of everyday favorites and new discoveries. Pick what you love.`,
        )}
      >
        <div className="flex gap-3 items-center">
          <Select
            aria-label={t(msg`Delivery date`)}
            value={cart.deliveryDate}
            onValueChange={(selectedValue) =>
              setCart({ ...cart, deliveryDate: selectedValue })
            }
          >
            {!deliveryDates(market).includes(cart.deliveryDate) && (
              <SelectItem value={cart.deliveryDate} disabled>
                {deliveryLabel(cart.deliveryDate, market, locale)} ·{" "}
                {t(msg`Unavailable`)}
              </SelectItem>
            )}
            {deliveryDates(market).map((d) => (
              <SelectItem key={d} value={d}>
                {deliveryLabel(d, market, locale)}
              </SelectItem>
            ))}
          </Select>
          <Link
            to={path("/plans?step=box")}
            className="text-xs underline whitespace-nowrap"
          >
            {t(msg`Delivery & box size`)}
          </Link>
        </div>
      </SectionTitle>
      <div className="menu-layout">
        <div id="choose-meals" tabIndex={-1}>
          <div className="filters">
            <ToggleGroup
              value={[filter]}
              onValueChange={(values) => {
                if (values.length) setFilter(String(values[0]));
              }}
              className="flex-wrap"
              aria-label={t(msg`Recipe categories`)}
            >
              {[
                ["all", t(msg`All recipes`)],
                ["classic", t(msg`Comfort classics`)],
                ["vegetarian", t(msg`Veggie favorites`)],
                ["quick", t(msg`Quick & easy`)],
              ].map(([id, label]) => (
                <ToggleGroupItem key={id} value={id} className="filter">
                  {label}
                </ToggleGroupItem>
              ))}
            </ToggleGroup>
            <Button
              variant="ghost"
              className="filter"
              aria-expanded={showAllergens}
              onClick={() => setShowAllergens(!showAllergens)}
            >
              <SlidersHorizontal size={13} />
              {t(msg`Allergens`)}
            </Button>
          </div>
          <Sheet open={showAllergens} onOpenChange={setShowAllergens}>
            <SheetContent side="bottom" className="navigation-sheet">
              <SheetTitle>{t(msg`Allergen preferences`)}</SheetTitle>
              <SheetDescription>
                {t(
                  msg`Exclude meals containing ingredients you want to avoid.`,
                )}
              </SheetDescription>
              <FieldSet className="notice">
                <FieldLegend>{t(msg`Exclude meals containing`)}</FieldLegend>
                <div className="flex flex-wrap gap-4 my-3">
                  {["milk", "wheat", "nuts", "fish", "soy", "sesame"].map(
                    (a) => (
                      <Label key={a} className="flex gap-2">
                        <Checkbox
                          checked={cart.exclude.includes(
                            a as Cart["exclude"][number],
                          )}
                          onCheckedChange={(e) =>
                            setCart({
                              ...cart,
                              exclude: (e
                                ? [...cart.exclude, a]
                                : cart.exclude.filter(
                                    (x) => x !== a,
                                  )) as Cart["exclude"],
                            })
                          }
                        />
                        {t(allergenLabels[a])}
                      </Label>
                    ),
                  )}
                </div>
                <p>
                  {t(
                    msg`Filters do not guarantee absence of cross-contact. Review each recipe and supplier labels before cooking.`,
                  )}
                </p>
              </FieldSet>
              <Button onClick={() => setShowAllergens(false)}>
                {t(msg`Show meals`)}
              </Button>
            </SheetContent>
          </Sheet>
          <Input
            className="mb-5"
            aria-label={t(msg`Search recipes`)}
            placeholder={t(msg`Find something delicious…`)}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
          {result.error ? (
            <ErrorState error={result.error} retry={result.refresh} />
          ) : !result.data ? (
            <Loading />
          ) : !result.data.menu ? (
            <Empty
              title={t(msg`No menu for this date`)}
              text={t(
                msg`Choose another delivery date to see available meals.`,
              )}
            >
              <CtaLink to={path("/plans")}>
                {t(msg`Choose a delivery date`)}
              </CtaLink>
            </Empty>
          ) : shown.length ? (
            <div className="meal-grid">
              {shown.map((r) => {
                const a = result.data!.availability.find(
                  (a) => a.recipeId === r.id,
                );
                return (
                  <RecipeCard
                    key={r.id}
                    recipe={r}
                    unavailable={
                      !!a && (!a.published || a.available < cart.servings)
                    }
                  />
                );
              })}
            </div>
          ) : (
            <Empty
              title={t(msg`No meals match just yet`)}
              text={t(msg`Try changing your filters or search.`)}
            >
              <Button
                variant="outline"
                onClick={() => {
                  setSearch("");
                  setFilter("all");
                }}
              >
                {t(msg`Clear search and category`)}
              </Button>
              {cart.exclude.length > 0 && (
                <p className="mt-3">
                  {t(msg`Your allergen exclusions are still applied.`)}
                </p>
              )}
            </Empty>
          )}
        </div>
        <BoxSummary />
      </div>
      <div className="mobile-box">
        <span className="text-xs">
          {t(msg`${cart.recipeIds.length}/${cart.mealCount} meals selected`)}
        </span>
        <Link className="cta" to={path("/box")}>
          {t(msg`Your box`)}
          <ArrowRight size={14} />
        </Link>
      </div>
    </section>
  );
}

export function RecipePage() {
  const { recipeId = "" } = useParams();
  const { catalog, catalogError, refreshCatalog } = useGather();
  const result = useLoad(
    async () => api.getRecipe({ slug: recipeId }),
    [recipeId],
  );
  if (catalogError)
    return <ErrorState error={catalogError} retry={refreshCatalog} />;
  if (!catalog) return <Loading />;
  const offered = catalog.recipes.find((recipe) => recipe.id === recipeId);
  if (offered)
    return <RecipeDetails key={offered.versionId} recipe={offered} />;
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  return <RecipeDetails key={result.data.versionId} recipe={result.data} />;
}

export function CookPage() {
  const { id = "", recipeId = "" } = useParams();
  const result = useLoad(
    async () => api.getCookingRecipe({ orderId: id, recipeId }),
    [id, recipeId],
  );
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const recipe = result.data.recipe;
  return (
    <RecipeDetails
      key={id + recipe.versionId}
      recipe={recipe}
      cooking={result.data}
    />
  );
}
function RecipeDetails({
  recipe: r,
  cooking,
}: {
  recipe: Recipe;
  cooking?: CookingRecipe;
}) {
  const {
    locale,
    cart,
    setCart,
    notice,
    market,
    path,
    catalog,
    preferredUnits,
  } = useGather();
  const { _: t } = useLingui();
  const { id } = useParams();
  const copy = recipeText(r, locale);
  const ingredientId = useId();
  const [done, setDone] = useState<number[]>([]);
  const [prepared, setPrepared] = useState<number[]>([]);
  const purchasedServings = cooking?.servings;
  const [browsingServings, setServings] = useState(cart.servings);
  const servings = purchasedServings ?? browsingServings;
  const [chosenUnits, setUnits] = useState<CookingUnits>();
  const units = chosenUnits ?? preferredUnits;
  const offered = Boolean(
    catalog?.recipes.some((recipe) => recipe.id === r.id),
  );
  return (
    <section className="section">
      <Link
        className="inline-block underline mb-6 text-sm no-print"
        to={path(purchasedServings ? `/orders/${id}` : "/menu")}
      >
        {purchasedServings ? t(msg`Back to order`) : t(msg`Back to menu`)}
      </Link>
      <div className="recipe-detail">
        <img src={`/media/${r.image}.png`} alt={recipeText(r, locale).name} />
        <div>
          <Badge>{recipeText(r, locale).tag}</Badge>
          <h1>{recipeText(r, locale).name}</h1>
          <p className="text-muted-foreground">
            {recipeText(r, locale).subtitle}
          </p>
          <div className="flex gap-6 my-6 text-sm">
            <span>
              {t(
                msg({
                  message: plural(r.minutes, {
                    one: "# minute",
                    other: "# minutes",
                  }),
                }),
              )}
            </span>
            <span>{t(msg`${r.calories} kcal`)}</span>
            <span>{t(msg`${r.protein}g protein`)}</span>
          </div>
          <p className="text-xs mb-6">
            {t(msg`Contains`)}:{" "}
            {new Intl.ListFormat(locale).format(
              r.allergens.map((allergen) => t(allergenLabels[allergen])),
            )}
            . {t(msg`Check supplier labels for cross-contact information.`)}
          </p>
          {!purchasedServings && !offered && (
            <p className="notice">
              {t(msg`This meal is unavailable in your delivery area.`)}{" "}
              <Link to={path("/menu")}>{t(msg`Explore the menu`)}</Link>
            </p>
          )}
          <div className="flex gap-3 no-print">
            {!purchasedServings && (
              <Button
                disabled={
                  !offered ||
                  (catalog?.availability.find((a) => a.recipeId === r.id)
                    ?.available ?? 0) < cart.servings
                }
                onClick={() => {
                  if (!offered) {
                    notice(
                      t(msg`This meal is unavailable in your delivery area.`),
                    );
                    return;
                  }
                  if (cart.recipeIds.includes(r.id)) {
                    notice(t(msg`Already in your box.`));
                    return;
                  }
                  if (cart.recipeIds.length >= cart.mealCount) {
                    notice(t(msg`Your box is full. Remove a meal first.`));
                    return;
                  }
                  setCart({ ...cart, recipeIds: [...cart.recipeIds, r.id] });
                  notice(t(msg`Added to your box.`));
                }}
              >
                {t(msg`Add to my box`)}
              </Button>
            )}
            <Button variant="outline" onClick={() => window.print()}>
              <Printer />
              {t(msg`Print recipe`)}
            </Button>
          </div>
        </div>
      </div>
      <div className="recipe-content">
        <div>
          <h2>{t(msg`Ingredients`)}</h2>
          {purchasedServings ? (
            <p className="text-sm mb-4">
              {t(
                msg({
                  message: plural(purchasedServings, {
                    one: "Ingredients for # person",
                    other: "Ingredients for # people",
                  }),
                }),
              )}
            </p>
          ) : (
            <ServingSlider
              label={t(msg`Servings`)}
              value={servings}
              onChange={setServings}
            />
          )}
          <div className="no-print">
            <ChoiceGroup
              label={t(msg`Ingredient units`)}
              value={units}
              onChange={setUnits}
              columns
              options={[
                { value: "metric", label: t(msg`Metric`) },
                { value: "us", label: t(msg`US measures`) },
                { value: "imperial", label: t(msg`UK measures`) },
              ]}
            />
          </div>
          <ul className="space-y-4 text-sm">
            {copy.ingredients.map((ingredient, n) => (
              <li key={n} className="border-b border-border pb-3">
                <Label
                  htmlFor={`${ingredientId}-${n}`}
                  className="flex items-start gap-3 font-normal cursor-pointer"
                >
                  <Checkbox
                    id={`${ingredientId}-${n}`}
                    className="mt-0.5 no-print"
                    checked={prepared.includes(n)}
                    onCheckedChange={(checked) =>
                      setPrepared((values) =>
                        checked
                          ? [...values, n]
                          : values.filter((value) => value !== n),
                      )
                    }
                  />
                  <span
                    className={`flex-1 min-w-0 ${prepared.includes(n) ? "text-muted-foreground line-through print:text-foreground print:no-underline" : ""}`}
                  >
                    {ingredient}
                  </span>
                  <IngredientQuantity
                    quantity={r.quantities[n]}
                    servings={servings}
                    baseServings={r.baseServings}
                    units={units}
                  />
                </Label>
              </li>
            ))}
          </ul>
          <h3 className="mt-7 mb-2">{t(msg`From your kitchen`)}</h3>
          <p className="text-xs text-muted-foreground">{copy.equipment}</p>
          <h3 className="mt-6">{t(msg`Pantry essentials`)}</h3>
          <p className="text-sm mt-2">{copy.pantry}</p>
          <h3 className="mt-6">{t(msg`Storage`)}</h3>
          <p className="text-sm mt-2">{copy.storage}</p>
          <p className="text-xs text-muted-foreground mt-6">
            {copy.nutritionBasis}
          </p>
        </div>
        <div>
          <h2>{t(msg`Let's get cooking`)}</h2>
          {copy.steps.map((step, i) => (
            <div
              className="cooking-step scroll-mt-6"
              key={i}
              id={`step-${i + 1}`}
              tabIndex={-1}
            >
              <Button
                variant="ghost"
                className="step-number"
                aria-label={
                  done.includes(i)
                    ? t(msg`Mark step ${i + 1} incomplete`)
                    : t(msg`Mark step ${i + 1} complete`)
                }
                aria-pressed={done.includes(i)}
                onClick={() =>
                  setDone(
                    done.includes(i)
                      ? done.filter((n) => n !== i)
                      : [...done, i],
                  )
                }
              >
                {done.includes(i) ? <Check size={15} /> : i + 1}
              </Button>
              <div className="min-w-0 flex-1">
                <h3 className="sr-only">{t(msg`Step ${i + 1}`)}</h3>
                <p
                  className={
                    done.includes(i)
                      ? "line-through text-muted-foreground print:text-foreground print:no-underline"
                      : ""
                  }
                >
                  {step}
                </p>
                <StepTimer
                  context={{
                    key: `${cooking?.orderId ?? "browse"}:${r.versionId}:${i}`,
                    step: i + 1,
                    recipeId: r.id,
                    orderId: cooking?.orderId,
                    market: cooking?.market ?? market,
                    name: { en: r.name, zh: r.translations.zh.name },
                  }}
                />
              </div>
            </div>
          ))}
          {cooking?.canReview && <RecipeFeedbackForm cooking={cooking} />}
        </div>
      </div>
    </section>
  );
}

export function Help() {
  const { path } = useGather();
  const { _: t } = useLingui();
  return (
    <section className="section max-w-3xl mx-auto">
      <SectionTitle
        eyebrow={t(msg`A LITTLE HELP`)}
        title={t(msg`Good questions. Simple answers.`)}
      />
      <Accordion>
        {[
          [
            t(msg`How does a weekly plan work?`),
            t(
              msg`Choose how many people you're cooking for, your meals and a delivery day. Manage your next box from My deliveries.`,
            ),
          ],
          [
            t(msg`Can I skip, pause or cancel?`),
            t(
              msg`Yes. Manage your plan in My deliveries. To change or cancel a confirmed box, open that order before its deadline. After that, contact us for help.`,
            ),
          ],
          [
            t(msg`Where do you deliver?`),
            t(
              msg`Enter your delivery area on Our plans to see whether we deliver to you and which days are available.`,
            ),
          ],
          [
            t(msg`What about food preferences and allergies?`),
            t(
              msg`Use the menu filters and review each recipe's allergens. Shared facilities can introduce cross-contact; filters are preferences and do not certify a meal is safe for an allergy.`,
            ),
          ],
          [
            t(msg`Something wrong with a delivery?`),
            t(
              msg`Open your order in My deliveries and choose Report an issue. Tell us what happened and we'll reply through your account.`,
            ),
          ],
        ].map(([q, a]) => (
          <AccordionItem value={q} key={q}>
            <AccordionTrigger className="py-5 text-base">{q}</AccordionTrigger>
            <AccordionContent className="text-muted-foreground leading-relaxed">
              {a}
            </AccordionContent>
          </AccordionItem>
        ))}
      </Accordion>
      <div className="mt-8">
        <CtaLink to={path("/account")}>{t(msg`Go to my deliveries`)}</CtaLink>
      </div>
    </section>
  );
}
