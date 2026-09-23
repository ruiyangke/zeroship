import { msg } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { useGather } from "../state";
import { useLoad } from "@gather/meal-kit/components/shared";
import { getCatalog } from "../api";
import type { Cart } from "@gather/meal-kit/domain";
import { boxSizeMessage } from "@gather/meal-kit/box-copy";
import { deliveryLabel } from "@gather/meal-kit/catalog";
import { recipeText } from "@gather/meal-kit/catalog-domain";
import { Card, CardHeader, CardContent } from "@gather/meal-kit/components/ui/card";
import { Button } from "@gather/meal-kit/components/ui/button";
import { Alert, AlertDescription } from "@gather/meal-kit/components/ui/alert";
import { Spinner } from "@gather/meal-kit/components/ui/spinner";
import {
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
} from "@gather/meal-kit/components/ui/dialog";

function DraftPreview({ cart, title }: { cart: Cart; title: string }) {
  const { locale } = useGather();
  const { _: t } = useLingui();
  const catalog = useLoad(
    async () => getCatalog({ market: cart.market, date: cart.deliveryDate }),
    [cart.market, cart.deliveryDate],
  );
  return (
    <Card className="gap-3">
      <CardHeader>
        <h3 className="font-semibold">{title}</h3>
      </CardHeader>
      <CardContent className="space-y-3">
        <p>{t(boxSizeMessage(cart.mealCount, cart.servings))}</p>
        <p>{deliveryLabel(cart.deliveryDate, cart.market, locale)}</p>
        <ul className="space-y-1 text-muted-foreground">
          {cart.recipeIds.map((id) => {
            const recipe = catalog.data?.recipes.find(
              (recipe) => recipe.id === id,
            );
            return (
              <li key={id}>
                {recipe
                  ? recipeText(recipe, locale).name
                  : catalog.data
                    ? t(msg`Unavailable meal`)
                    : t(msg`Loading…`)}
              </li>
            );
          })}
        </ul>
        {!cart.recipeIds.length && <p>{t(msg`No meals selected yet.`)}</p>}
      </CardContent>
    </Card>
  );
}

export function DraftFeedback() {
  const { draft, retryDraft, resolveDraft } = useGather();
  const { _: t } = useLingui();
  const conflict = draft.conflict;
  return (
    <>
      {draft.error && !conflict && (
        <Alert className="my-5">
          <AlertDescription className="flex flex-wrap items-center justify-between gap-3">
            {t(draft.error)}
            <Button variant="outline" onClick={() => void retryDraft()}>
              {t(msg`Try again`)}
            </Button>
          </AlertDescription>
        </Alert>
      )}
      {!draft.ready && !draft.error && (
        <p
          role="status"
          className="flex items-center gap-2 mt-5 text-sm text-muted-foreground"
        >
          <Spinner aria-hidden="true" />
          {t(msg`Loading your saved box…`)}
        </p>
      )}
      <Dialog open={!!conflict}>
        <DialogContent showCloseButton={false} className="sm:max-w-2xl">
          <DialogTitle>
            {conflict?.guest
              ? t(msg`You have a saved box`)
              : t(msg`Your box changed elsewhere`)}
          </DialogTitle>
          <DialogDescription>
            {conflict?.guest
              ? t(
                  msg`Choose which box to keep in your account. Your existing orders won't change.`,
                )
              : t(
                  msg`Another tab or device updated your box. Review both versions before continuing.`,
                )}
          </DialogDescription>
          {conflict && (
            <div className="grid gap-4 sm:grid-cols-2">
              <DraftPreview
                cart={conflict.guest?.cart ?? draft.cart}
                title={t(msg`This box`)}
              />
              {conflict.draft ? (
                <DraftPreview
                  cart={conflict.draft.cart}
                  title={t(msg`Saved box`)}
                />
              ) : (
                <p>{t(msg`Your saved box is empty.`)}</p>
              )}
            </div>
          )}
          {draft.error && (
            <Alert>
              <AlertDescription>{t(draft.error)}</AlertDescription>
            </Alert>
          )}
          <div className="flex flex-wrap gap-3">
            <Button
              disabled={draft.saving}
              onClick={() => void resolveDraft("local")}
            >
              {t(msg`Keep this box`)}
            </Button>
            <Button
              variant="outline"
              disabled={draft.saving}
              onClick={() => void resolveDraft("saved")}
            >
              {t(msg`Use saved box`)}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
