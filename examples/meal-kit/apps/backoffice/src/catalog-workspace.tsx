import { SelectItem } from "@gather/meal-kit/components/select-field";
import { Tabs, TabsList, TabsTrigger } from "@gather/meal-kit/components/ui/tabs";
import { AnimatedTabsContent as TabsContent } from "@gather/meal-kit/components/animated-tabs-content";
import { FieldSet, FieldLegend } from "@gather/meal-kit/components/ui/field";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Textarea } from "@gather/meal-kit/components/ui/textarea";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import { useState } from "react";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { useGather } from "./state";
import * as api from "./api";
import {
  markets,
  deliveryDates,
  cutoffForDate,
  deliveryLabel,
  money,
  allergenLabels,
  type Locale,
} from "@gather/meal-kit/catalog";
import { languageName } from "@gather/meal-kit/locales";
import {
  recipeDraftInputSchema,
  recipeText,
  type RecipeDraft,
  type MenuDraft,
} from "@gather/meal-kit/catalog-domain";
import {
  Badge,
  Button,
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
  Field,
  Input,
  Select,
  Loading,
  ErrorState,
  useLoad,
} from "@gather/meal-kit/components/shared";

type Workspace = Awaited<ReturnType<typeof api.getCatalogWorkspace>>;
type RecipeRow = Workspace["recipes"][number];
type MenuRow = Workspace["menus"][number];
const emptyText = {
  name: "",
  subtitle: "",
  tag: "",
  equipment: "",
  ingredients: [""],
  steps: [""],
  pantry: "",
  storage: "",
  nutritionBasis: "",
};
const emptyRecipe: RecipeDraft = {
  ...emptyText,
  category: "classic",
  minutes: 30,
  calories: 0,
  protein: 0,
  image: "chicken",
  allergens: [],
  baseServings: 2,
  quantities: [{ amount: 1, unit: "g" }],
  translations: { zh: { ...emptyText } },
};

export function CatalogWorkspace() {
  const { market, locale, act, notice } = useGather();
  const { _: t } = useLingui();
  const result = useLoad(
    async () => api.getCatalogWorkspace({ market }),
    [market],
  );
  const [tab, setTab] = useState("menus");
  const [recipeId, setRecipeId] = useState("");
  const [date, setDate] = useState(deliveryDates(market)[0]);
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const data = result.data;
  const activeTab = !data.canEditMenus
    ? "recipes"
    : !data.canEditRecipes
      ? "menus"
      : tab;
  const row = data.recipes.find((recipe) => recipe.id === recipeId);
  const menu = data.menus.find((menu) => menu.delivery_date === date);
  return (
    <div>
      <Tabs value={activeTab} onValueChange={(value) => setTab(String(value))}>
        <TabsList className="mb-6" aria-label={t(msg`Catalog sections`)}>
          {data.canEditMenus && (
            <TabsTrigger value="menus">{t(msg`Dated menus`)}</TabsTrigger>
          )}
          {data.canEditRecipes && (
            <TabsTrigger value="recipes">{t(msg`Recipe library`)}</TabsTrigger>
          )}
        </TabsList>
        <TabsContent value={activeTab}>
          {activeTab === "menus" ? (
            <>
              <Field label={t(msg`Menu delivery date`)}>
                <Select
                  value={date}
                  onValueChange={(selectedValue) => setDate(selectedValue)}
                >
                  {deliveryDates(market).map((value) => (
                    <SelectItem key={value} value={value}>
                      {deliveryLabel(value, market, locale)}
                    </SelectItem>
                  ))}
                </Select>
              </Field>
              <MenuEditor
                key={`${market}:${date}:${menu?.version}`}
                date={date}
                row={menu}
                data={data}
                saved={result.refresh}
              />
            </>
          ) : (
            <>
              <Field label={t(msg`Recipe`)}>
                <Select
                  value={recipeId}
                  onValueChange={(selectedValue) => setRecipeId(selectedValue)}
                >
                  <SelectItem value="">{t(msg`New recipe`)}</SelectItem>
                  {data.recipes.map((recipe) => (
                    <SelectItem key={recipe.id} value={recipe.id}>
                      {recipeText(recipe.draft as RecipeDraft, locale).name ||
                        recipe.slug}
                    </SelectItem>
                  ))}
                </Select>
              </Field>
              {row && (
                <div className="flex flex-wrap items-center gap-3 mb-5">
                  <Badge>
                    {row.archived ? t(msg`Archived`) : t(msg`Editable draft`)}
                  </Badge>
                  <span className="text-sm">
                    {t(msg`Approved versions`)}:{" "}
                    {data.versions
                      .filter((v) => v.recipe_id === row.id)
                      .map((v) => v.revision)
                      .join(", ") || "—"}
                  </span>
                  <Button
                    variant="outline"
                    onClick={() =>
                      act(async () => {
                        await api.archiveRecipe({
                          id: row.id,
                          version: row.version,
                          archived: !row.archived,
                        });
                        result.refresh();
                      })
                    }
                  >
                    {row.archived
                      ? t(msg`Restore recipe`)
                      : t(msg`Archive recipe`)}
                  </Button>
                </div>
              )}
              {!row?.archived && (
                <RecipeEditor
                  key={row ? row.id + ":" + row.version : "new"}
                  row={row}
                  saved={(id) => {
                    setRecipeId(id);
                    result.refresh();
                    notice(t(msg`Recipe draft saved.`));
                  }}
                />
              )}
            </>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}

function RecipeEditor({
  row,
  saved,
}: {
  row?: RecipeRow;
  saved: (id: string) => void;
}) {
  const { _: t } = useLingui();
  const { act, busy, notice } = useGather();
  const [draft, setDraft] = useState<RecipeDraft>(
    row ? (row.draft as RecipeDraft) : structuredClone(emptyRecipe),
  );
  const [slug, setSlug] = useState(row?.slug ?? "");
  const [language, setLanguage] = useState<Locale>("en");
  const [error, setError] = useState("");
  const [approval, setApproval] = useState(false);
  const [note, setNote] = useState("");
  const copy = recipeText(draft, language);
  const dirty = JSON.stringify(draft) !== JSON.stringify(row?.draft);
  function setText(field: keyof typeof emptyText, value: string | string[]) {
    setDraft((old) =>
      language === "en"
        ? { ...old, [field]: value }
        : {
            ...old,
            translations: { zh: { ...old.translations.zh, [field]: value } },
          },
    );
  }
  return (
    <>
      <form
        className="panel space-y-6"
        noValidate
        onSubmit={(event) => {
          event.preventDefault();
          setError("");
          const parsed = recipeDraftInputSchema.safeParse(draft);
          if (!parsed.success || !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(slug)) {
            setError(
              t(
                msg`Check the recipe link, field lengths and ingredient quantities.`,
              ),
            );
            return;
          }
          void act(async () => {
            const result = await api.saveRecipeDraft({
              id: row?.id,
              version: row?.version,
              slug,
              draft: parsed.data,
            });
            saved(result.id);
          });
        }}
      >
        <div className="grid md:grid-cols-2 gap-x-6">
          <Field
            label={t(msg`Recipe link`)}
            hint={t(msg`Use lowercase words separated by hyphens.`)}
          >
            <Input
              required
              pattern="[a-z0-9]+(-[a-z0-9]+)*"
              value={slug}
              disabled={!!row}
              onChange={(e) => setSlug(e.target.value)}
            />
          </Field>
          <Field label={t(msg`Content language`)}>
            <Select
              value={language}
              onValueChange={(selectedValue) =>
                setLanguage(selectedValue as Locale)
              }
            >
              {(["en", "zh"] as const).map((value) => (
                <SelectItem key={value} value={value}>
                  {languageName(value)}
                </SelectItem>
              ))}
            </Select>
          </Field>
        </div>
        <div className="grid md:grid-cols-2 gap-x-6">
          {(
            [
              ["name", t(msg`Recipe name`)],
              ["subtitle", t(msg`Description`)],
              ["tag", t(msg`Recipe tag`)],
              ["equipment", t(msg`Equipment`)],
              ["pantry", t(msg`Pantry essentials`)],
              ["storage", t(msg`Storage`)],
              ["nutritionBasis", t(msg`Nutrition basis`)],
            ] as const
          ).map(([field, label]) => (
            <Field key={field} label={label}>
              <Input
                required
                value={copy[field]}
                onChange={(e) => setText(field, e.target.value)}
              />
            </Field>
          ))}
        </div>
        <div className="grid sm:grid-cols-3 gap-x-5">
          <Field label={t(msg`Preparation time (minutes)`)}>
            <Input
              required
              type="number"
              min="1"
              max="600"
              value={draft.minutes}
              onChange={(e) =>
                setDraft({ ...draft, minutes: Number(e.target.value) })
              }
            />
          </Field>
          <Field label={t(msg`Calories per serving`)}>
            <Input
              required
              type="number"
              min="0"
              value={draft.calories}
              onChange={(e) =>
                setDraft({ ...draft, calories: Number(e.target.value) })
              }
            />
          </Field>
          <Field label={t(msg`Protein per serving (g)`)}>
            <Input
              required
              type="number"
              min="0"
              step="0.1"
              value={draft.protein}
              onChange={(e) =>
                setDraft({ ...draft, protein: Number(e.target.value) })
              }
            />
          </Field>
          <Field label={t(msg`Recipe category`)}>
            <Select
              value={draft.category}
              onValueChange={(selectedValue) =>
                setDraft({
                  ...draft,
                  category: selectedValue as RecipeDraft["category"],
                })
              }
            >
              <SelectItem value="classic">
                {t(msg`Comfort classics`)}
              </SelectItem>
              <SelectItem value="vegetarian">
                {t(msg`Veggie favorites`)}
              </SelectItem>
              <SelectItem value="quick">{t(msg`Quick & easy`)}</SelectItem>
            </Select>
          </Field>
          <Field label={t(msg`Photo`)}>
            <Select
              value={draft.image}
              onValueChange={(selectedValue) =>
                setDraft({
                  ...draft,
                  image: selectedValue as RecipeDraft["image"],
                })
              }
            >
              {["chicken", "pasta", "salmon", "bowl", "mushroom", "tofu"].map(
                (image) => (
                  <SelectItem key={image} value={image}>
                    {t(
                      {
                        chicken: msg`Chicken`,
                        pasta: msg`Pasta`,
                        salmon: msg`Salmon`,
                        bowl: msg`Harvest bowl`,
                        mushroom: msg`Mushrooms`,
                        tofu: msg`Tofu`,
                      }[image as RecipeDraft["image"]],
                    )}
                  </SelectItem>
                ),
              )}
            </Select>
          </Field>
          <Field label={t(msg`Quantities serve`)}>
            <Input
              required
              type="number"
              min="1"
              max="20"
              value={draft.baseServings}
              onChange={(e) =>
                setDraft({ ...draft, baseServings: Number(e.target.value) })
              }
            />
          </Field>
        </div>
        <FieldSet>
          <FieldLegend className="font-medium mb-3">
            {t(msg`Contains allergens`)}
          </FieldLegend>
          <div className="flex flex-wrap gap-4">
            {Object.entries(allergenLabels).map(([id, label]) => (
              <Label key={id} className="flex gap-2 text-sm">
                <Checkbox
                  checked={draft.allergens.includes(id as never)}
                  onCheckedChange={(e) =>
                    setDraft({
                      ...draft,
                      allergens: e
                        ? [
                            ...draft.allergens,
                            id as RecipeDraft["allergens"][number],
                          ]
                        : draft.allergens.filter((value) => value !== id),
                    })
                  }
                />
                {t(label)}
              </Label>
            ))}
          </div>
        </FieldSet>
        <FieldSet>
          <FieldLegend className="font-medium mb-3">
            {t(msg`Ingredients and quantities`)}
          </FieldLegend>
          {draft.quantities.map((quantity, index) => (
            <div
              className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_90px_90px_auto] items-start"
              key={index}
            >
              <Field label={t(msg`Ingredient ${index + 1}`)}>
                <Input
                  required
                  value={copy.ingredients[index] ?? ""}
                  onChange={(e) =>
                    setText(
                      "ingredients",
                      copy.ingredients.map((text, i) =>
                        i === index ? e.target.value : text,
                      ),
                    )
                  }
                />
              </Field>
              <Field label={t(msg`Amount`)}>
                <Input
                  required
                  type="number"
                  min="0.01"
                  step="0.01"
                  value={quantity.amount}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      quantities: draft.quantities.map((value, i) =>
                        i === index
                          ? { ...value, amount: Number(e.target.value) }
                          : value,
                      ),
                    })
                  }
                />
              </Field>
              <Field label={t(msg`Unit`)}>
                <Select
                  value={quantity.unit}
                  onValueChange={(selectedValue) =>
                    setDraft({
                      ...draft,
                      quantities: draft.quantities.map((value, i) =>
                        i === index
                          ? {
                              ...value,
                              unit: selectedValue as typeof quantity.unit,
                            }
                          : value,
                      ),
                    })
                  }
                >
                  <SelectItem value="g">g</SelectItem>
                  <SelectItem value="ml">ml</SelectItem>
                  <SelectItem value="piece">{t(msg`pieces`)}</SelectItem>
                </Select>
              </Field>
              <Button
                className="mt-7"
                type="button"
                variant="ghost"
                disabled={draft.quantities.length === 1}
                aria-label={t(msg`Remove ingredient ${index + 1}`)}
                onClick={() =>
                  setDraft({
                    ...draft,
                    ingredients: draft.ingredients.filter(
                      (_, i) => i !== index,
                    ),
                    quantities: draft.quantities.filter((_, i) => i !== index),
                    translations: {
                      zh: {
                        ...draft.translations.zh,
                        ingredients: draft.translations.zh.ingredients.filter(
                          (_, i) => i !== index,
                        ),
                      },
                    },
                  })
                }
              >
                ×
              </Button>
            </div>
          ))}
          <Button
            type="button"
            variant="outline"
            onClick={() =>
              setDraft({
                ...draft,
                ingredients: [...draft.ingredients, ""],
                quantities: [...draft.quantities, { amount: 1, unit: "g" }],
                translations: {
                  zh: {
                    ...draft.translations.zh,
                    ingredients: [...draft.translations.zh.ingredients, ""],
                  },
                },
              })
            }
          >
            {t(msg`Add ingredient`)}
          </Button>
        </FieldSet>
        <FieldSet>
          <FieldLegend className="font-medium mb-3">
            {t(msg`Cooking steps`)}
          </FieldLegend>
          {copy.steps.map((step, index) => (
            <div className="flex gap-2" key={index}>
              <div className="flex-1">
                <Field label={t(msg`Step ${index + 1}`)}>
                  <Textarea
                    required
                    value={step}
                    onChange={(e) =>
                      setText(
                        "steps",
                        copy.steps.map((text, i) =>
                          i === index ? e.target.value : text,
                        ),
                      )
                    }
                  />
                </Field>
              </div>
              <Button
                type="button"
                variant="ghost"
                className="mt-7"
                disabled={copy.steps.length === 1}
                aria-label={t(msg`Remove step ${index + 1}`)}
                onClick={() =>
                  setDraft({
                    ...draft,
                    steps: draft.steps.filter((_, i) => i !== index),
                    translations: {
                      zh: {
                        ...draft.translations.zh,
                        steps: draft.translations.zh.steps.filter(
                          (_, i) => i !== index,
                        ),
                      },
                    },
                  })
                }
              >
                ×
              </Button>
            </div>
          ))}
          <Button
            type="button"
            variant="outline"
            onClick={() =>
              setDraft({
                ...draft,
                steps: [...draft.steps, ""],
                translations: {
                  zh: {
                    ...draft.translations.zh,
                    steps: [...draft.translations.zh.steps, ""],
                  },
                },
              })
            }
          >
            {t(msg`Add cooking step`)}
          </Button>
        </FieldSet>
        {error && (
          <p role="alert" className="error-panel">
            {error}
          </p>
        )}
        <div className="flex flex-wrap gap-3">
          <Button type="submit" disabled={busy}>
            {t(msg`Save recipe draft`)}
          </Button>
          <Button
            type="button"
            variant="outline"
            disabled={busy || !row || dirty}
            onClick={() => setApproval(true)}
          >
            {t(msg`Approve saved recipe`)}
          </Button>
        </div>
        <p className="text-sm text-muted-foreground">
          {t(
            msg`Approval creates a fixed version. Publish that version in a menu when it is ready to sell.`,
          )}
        </p>
      </form>
      <Dialog open={approval} onOpenChange={setApproval}>
        <DialogContent>
          <DialogTitle>{t(msg`Approve recipe`)}</DialogTitle>
          <DialogDescription>
            {t(
              msg`Review ingredients, allergens, quantities, nutrition and both languages. Approved versions cannot be edited.`,
            )}
          </DialogDescription>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              if (row)
                void act(async () => {
                  await api.approveRecipe({
                    id: row.id,
                    version: row.version,
                    note,
                  });
                  setApproval(false);
                  saved(row.id);
                  notice(t(msg`Recipe approved.`));
                });
            }}
          >
            <Field label={t(msg`Review notes`)}>
              <Textarea
                required
                minLength={5}
                maxLength={1000}
                value={note}
                onChange={(e) => setNote(e.target.value)}
              />
            </Field>
            <Button type="submit" disabled={busy}>
              {t(msg`Confirm approval`)}
            </Button>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}

function MenuEditor({
  row,
  date,
  data,
  saved,
}: {
  row?: MenuRow;
  date: string;
  data: Workspace;
  saved: () => void;
}) {
  const { market, locale, act, busy, notice } = useGather();
  const { _: t } = useLingui();
  const [draft, setDraft] = useState<MenuDraft>(
    row
      ? (row.draft as MenuDraft)
      : {
          price: markets[market].price,
          shipping: markets[market].shipping,
          opensAt: new Date().toISOString(),
          closesAt: cutoffForDate(market, date),
          offerings: [],
        },
  );
  const dirty = JSON.stringify(draft) !== JSON.stringify(row?.draft);
  const perform = (fn: () => Promise<unknown>) =>
    act(async () => {
      await fn();
      saved();
    });
  return (
    <form
      className="panel"
      onSubmit={(event) => {
        event.preventDefault();
        void perform(async () => {
          await api.saveMenuDraft({
            market,
            date,
            id: row?.id,
            version: row?.version,
            draft,
          });
          notice(t(msg`Menu draft saved.`));
        });
      }}
    >
      <div className="flex gap-3 mb-5">
        <Badge>
          {row?.status === "published"
            ? t(msg`Published`)
            : row?.status === "withdrawn"
              ? t(msg`Withdrawn`)
              : t(msg`Draft`)}
        </Badge>
        <span className="text-sm">
          {markets[market].currency} · {t(markets[market].name)}
        </span>
      </div>
      <div className="grid sm:grid-cols-2 gap-x-6">
        <Field
          label={t(msg`Base price per serving`)}
          hint={markets[market].currency}
        >
          <Input
            required
            type="number"
            min="0.01"
            step="0.01"
            value={draft.price / 100}
            onChange={(e) =>
              setDraft({
                ...draft,
                price: Math.round(Number(e.target.value) * 100),
              })
            }
          />
        </Field>
        <Field label={t(msg`Delivery fee`)} hint={markets[market].currency}>
          <Input
            required
            type="number"
            min="0"
            step="0.01"
            value={draft.shipping / 100}
            onChange={(e) =>
              setDraft({
                ...draft,
                shipping: Math.round(Number(e.target.value) * 100),
              })
            }
          />
        </Field>
        <Field label={t(msg`Sale opens (UTC)`)}>
          <Input
            required
            type="datetime-local"
            value={draft.opensAt.slice(0, 16)}
            onChange={(e) => {
              if (e.target.value)
                setDraft({
                  ...draft,
                  opensAt: new Date(e.target.value + ":00Z").toISOString(),
                });
            }}
          />
        </Field>
        <Field label={t(msg`Sale closes (UTC)`)}>
          <Input
            required
            type="datetime-local"
            value={draft.closesAt.slice(0, 16)}
            onChange={(e) => {
              if (e.target.value)
                setDraft({
                  ...draft,
                  closesAt: new Date(e.target.value + ":00Z").toISOString(),
                });
            }}
          />
        </Field>
      </div>
      <p className="text-sm text-muted-foreground mb-5">
        {t(
          msg`Select approved recipes and set any extra charge per serving. New offerings need available inventory before customers can order.`,
        )}
      </p>
      <div className="space-y-4 mb-6">
        {data.recipes
          .filter((recipe) => !recipe.archived)
          .map((recipe) => {
            const versions = data.versions.filter(
              (version) => version.recipe_id === recipe.id,
            );
            if (!versions.length) return null;
            const offering = draft.offerings.find((offer) =>
              versions.some((version) => version.id === offer.recipeVersionId),
            );
            const copy = recipeText(recipe.draft as RecipeDraft, locale);
            function update(value: {
              recipeVersionId: string;
              premium: number;
            }) {
              setDraft({
                ...draft,
                offerings: draft.offerings.map((offer) =>
                  offer === offering ? value : offer,
                ),
              });
            }
            return (
              <div
                key={recipe.id}
                className="border border-border rounded-lg p-4"
              >
                <Label className="flex items-center gap-3 font-medium">
                  <Checkbox
                    checked={!!offering}
                    onCheckedChange={(e) =>
                      setDraft({
                        ...draft,
                        offerings: e
                          ? [
                              ...draft.offerings,
                              { recipeVersionId: versions[0].id, premium: 0 },
                            ]
                          : draft.offerings.filter(
                              (offer) => offer !== offering,
                            ),
                      })
                    }
                  />
                  {copy.name}
                </Label>
                {offering && (
                  <div className="grid sm:grid-cols-2 gap-5 mt-4">
                    <Field label={t(msg`Approved version`)}>
                      <Select
                        value={offering.recipeVersionId}
                        onValueChange={(selectedValue) =>
                          update({
                            ...offering,
                            recipeVersionId: selectedValue,
                          })
                        }
                      >
                        {versions.map((version) => (
                          <SelectItem key={version.id} value={version.id}>
                            {version.revision} ·{" "}
                            {
                              recipeText(version.content as RecipeDraft, locale)
                                .name
                            }
                          </SelectItem>
                        ))}
                      </Select>
                    </Field>
                    <Field
                      label={t(msg`Extra per serving`)}
                      hint={markets[market].currency}
                    >
                      <Input
                        required
                        type="number"
                        min="0"
                        step="0.01"
                        value={offering.premium / 100}
                        onChange={(e) =>
                          update({
                            ...offering,
                            premium: Math.round(Number(e.target.value) * 100),
                          })
                        }
                      />
                    </Field>
                  </div>
                )}
              </div>
            );
          })}
      </div>
      <div className="flex flex-wrap gap-3">
        <Button type="submit" disabled={busy}>
          {t(msg`Save menu draft`)}
        </Button>
        <Button
          type="button"
          disabled={busy || !row || dirty}
          variant="outline"
          onClick={() =>
            row &&
            perform(async () => {
              await api.publishMenu({ id: row.id, version: row.version });
              notice(t(msg`Menu published.`));
            })
          }
        >
          {t(msg`Publish saved menu`)}
        </Button>
        {row?.status === "published" && (
          <Button
            type="button"
            variant="outline"
            disabled={busy}
            onClick={() =>
              perform(async () => {
                await api.withdrawMenu({ id: row.id, version: row.version });
                notice(t(msg`Menu withdrawn. Existing orders are unchanged.`));
              })
            }
          >
            {t(msg`Withdraw menu`)}
          </Button>
        )}
      </div>
      <p className="mt-4 text-sm text-muted-foreground">
        {t(
          msg`Save changes before publishing. Customers see the last published version until you publish again or withdraw the menu.`,
        )}
      </p>
    </form>
  );
}
