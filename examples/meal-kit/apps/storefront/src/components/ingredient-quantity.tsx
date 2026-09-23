import { useLingui } from "@lingui/react";
import { msg, plural } from "@lingui/core/macro";
import { useGather } from "../state";
import {
  ingredientAmount,
  type CookingUnits,
  type IngredientAmount,
} from "@gather/meal-kit/cooking-domain";

export function IngredientQuantity({
  quantity,
  servings,
  baseServings,
  units,
}: {
  quantity: IngredientAmount;
  servings: number;
  baseServings: number;
  units: CookingUnits;
}) {
  const { locale } = useGather();
  const { _: t } = useLingui();
  const converted = ingredientAmount(quantity, servings, baseServings, units);
  const amount = new Intl.NumberFormat(locale, {
    maximumSignificantDigits: 4,
  }).format(converted.amount);
  const label =
    converted.unit === "g"
      ? t(msg`${amount} g`)
      : converted.unit === "ml"
        ? t(msg`${amount} ml`)
        : converted.unit === "oz"
          ? t(msg`${amount} oz`)
          : converted.unit === "us_fl_oz"
            ? t(msg`${amount} US fl oz`)
            : converted.unit === "uk_fl_oz"
              ? t(msg`${amount} UK fl oz`)
              : t(
                  msg({
                    message: plural(converted.amount, {
                      one: "# piece",
                      other: "# pieces",
                    }),
                  }),
                );
  return (
    <span
      className="whitespace-nowrap font-normal tabular-nums"
      data-ingredient-quantity
    >
      {label}
    </span>
  );
}
