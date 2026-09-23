import { recipeText } from "@gather/meal-kit/catalog-domain";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { Link } from "react-router-dom";
import {
Check,
Clock3,
Heart,
Leaf,
Plus
} from "lucide-react";
import { Button } from "@gather/meal-kit/components/ui/button";
import { useGather } from "../state";
import { money,type Recipe } from "@gather/meal-kit/catalog";
import { cn } from "@gather/meal-kit/lib/utils";

export function RecipeCard({
  recipe,
  unavailable = false,
  onDetail,
  compact = false,
}: {
  recipe: Recipe;
  unavailable?: boolean;
  onDetail?: () => void;
  compact?: boolean;
}) {
  const {
    cart,
    setCart,
    locale,
    market,
    path,
    notice,
    favorites,
    toggleFavorite,
    busy,
  } = useGather();
  const { _: t } = useLingui();
  const selected = cart.recipeIds.includes(recipe.id);
  const favorite = favorites.includes(recipe.id);
  const toggle = () => {
    if (!selected && cart.recipeIds.length >= cart.mealCount) {
      notice(t(msg`Your box is full. Remove a meal or choose a larger box.`));
      return;
    }
    setCart((c) => ({
      ...c,
      recipeIds: selected
        ? c.recipeIds.filter((id) => id !== recipe.id)
        : [...c.recipeIds, recipe.id],
    }));
  };
  return (
    <article className={cn("meal-card", selected && "meal-card-selected")}>
      <div className="meal-photo">
        <Link to={path(`/recipes/${recipe.id}`)} onClick={onDetail}>
          <img
            src={`/media/${recipe.image}.png`}
            alt={recipeText(recipe, locale).name}
            loading="lazy"
          />
        </Link>
        <span className="photo-tag">{recipeText(recipe, locale).tag}</span>
        <Button
          variant="ghost"
          type="button"
          className={cn("favorite", favorite && "is-favorite")}
          aria-label={t(msg`Save recipe`)}
          aria-pressed={favorite}
          disabled={busy}
          onClick={() => {
            void toggleFavorite(recipe.id);
          }}
        >
          <Heart size={17} fill={favorite ? "currentColor" : "none"} />
        </Button>
        {unavailable && <div className="sold-out">{t(msg`Sold out`)}</div>}
      </div>
      <div className="meal-body">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Clock3 size={13} />
          {t(msg`${recipe.minutes} min`)}
          <span className="mx-1">·</span>
          {t(msg`${recipe.calories} kcal`)}
          {recipe.category === "vegetarian" && (
            <Leaf size={13} className="text-primary" />
          )}
        </div>
        <Link to={path(`/recipes/${recipe.id}`)}>
          <h3>{recipeText(recipe, locale).name}</h3>
        </Link>
        <p>{recipeText(recipe, locale).subtitle}</p>
        <div className="meal-card-bottom">
          <span className="text-xs text-muted-foreground">
            {recipe.premium
              ? `+${money(recipe.premium, market, locale)} ${t(msg`/ serving`)}`
              : t(msg`Included in your plan`)}
          </span>
          {!compact && (
            <Button
              aria-label={
                selected
                  ? t(msg`Remove ${recipeText(recipe, locale).name}`)
                  : t(msg`Add ${recipeText(recipe, locale).name}`)
              }
              variant={selected ? "default" : "outline"}
              size="icon"
              className="rounded-full"
              disabled={unavailable && !selected}
              onClick={toggle}
            >
              {selected ? <Check size={18} /> : <Plus size={18} />}
            </Button>
          )}
        </div>
      </div>
    </article>
  );
}
