import { useRef, useState } from "react";
import { useLingui } from "@lingui/react";
import { msg, plural } from "@lingui/core/macro";
import { Star } from "lucide-react";
import * as api from "../api";
import { useGather } from "../state";
import type { CookingRecipe, RecipeFeedback } from "@gather/meal-kit/cooking-domain";
import { Button, ErrorState, Empty, Field, Loading, useLoad } from "@gather/meal-kit/components/shared";
import { RadioGroup, RadioGroupItem } from "@gather/meal-kit/components/ui/radio-group";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Textarea } from "@gather/meal-kit/components/ui/textarea";
import { Card } from "@gather/meal-kit/components/ui/card";
import { ChoiceGroup } from "@gather/meal-kit/components/choice-group";
import { FieldSet } from "@gather/meal-kit/components/ui/field";

export function RecipeFeedbackForm({ cooking }: { cooking: CookingRecipe }) {
  const { _: t } = useLingui();
  const { locale } = useGather();
  const [saved, setSaved] = useState<RecipeFeedback | null>(cooking.feedback);
  const [rating, setRating] = useState(cooking.feedback?.rating ?? 0);
  const [cookAgain, setCookAgain] = useState(
    cooking.feedback?.cookAgain ?? null,
  );
  const [comment, setComment] = useState(cooking.feedback?.comment ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [success, setSuccess] = useState(false);
  const attempt = useRef<{ signature: string; requestKey: string } | null>(
    null,
  );
  const edit = () => setSuccess(false);
  async function loadSaved() {
    setBusy(true);
    try {
      const current = await api.getCookingRecipe({
        orderId: cooking.orderId,
        recipeId: cooking.recipe.id,
      });
      setSaved(current.feedback);
      setRating(current.feedback?.rating ?? 0);
      setCookAgain(current.feedback?.cookAgain ?? null);
      setComment(current.feedback?.comment ?? "");
      attempt.current = null;
      setError("");
      setSuccess(false);
    } catch (error) {
      setError(String((error as Error).message));
    } finally {
      setBusy(false);
    }
  }
  return (
    <Card className="mt-10 p-6 no-print">
      <form
        onSubmit={async (event) => {
          event.preventDefault();
          if (!rating || busy) return;
          setBusy(true);
          setError("");
          setSuccess(false);
          const content = {
            orderId: cooking.orderId,
            recipeId: cooking.recipe.id,
            rating,
            cookAgain,
            comment,
            expectedVersion: saved?.version ?? null,
          };
          const signature = JSON.stringify(content);
          if (attempt.current?.signature !== signature)
            attempt.current = { signature, requestKey: crypto.randomUUID() };
          try {
            const feedback = await api.saveRecipeFeedback({
              ...content,
              requestKey: attempt.current.requestKey,
            });
            setSaved(feedback);
            setSuccess(true);
            attempt.current = null;
          } catch (error) {
            setError(String((error as Error).message));
          } finally {
            setBusy(false);
          }
        }}
      >
        <h2 className="mb-2">{t(msg`How was this meal?`)}</h2>
        <p className="text-sm text-muted-foreground mb-5">
          {t(msg`Share your feedback with our kitchen.`)}
        </p>
        <RadioGroup
          className="grid grid-cols-5 gap-2 mb-5"
          aria-label={t(msg`Meal rating`)}
          value={rating ? String(rating) : ""}
          onValueChange={(value) => {
            setRating(Number(value));
            edit();
          }}
          disabled={busy}
        >
          {[1, 2, 3, 4, 5].map((value) => (
            <Label
              key={value}
              className="relative flex min-h-14 cursor-pointer flex-col justify-center gap-1 rounded-lg border p-2 has-[:checked]:border-primary has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-ring"
            >
              <RadioGroupItem
                value={String(value)}
                aria-label={t(
                  msg({
                    message: plural(value, { one: "# star", other: "# stars" }),
                  }),
                )}
                className="absolute inset-0 z-10 size-full aspect-auto rounded-lg opacity-0 after:hidden"
              />
              <Star
                aria-hidden="true"
                className={`size-5 ${value <= rating ? "fill-primary text-primary" : "text-muted-foreground"}`}
              />
              <span aria-hidden="true">
                {new Intl.NumberFormat(locale).format(value)}
              </span>
            </Label>
          ))}
        </RadioGroup>
        <FieldSet disabled={busy}>
          <ChoiceGroup
            label={t(msg`Would you cook it again?`)}
            columns
            value={cookAgain === null ? "unsure" : cookAgain ? "yes" : "no"}
            onChange={(value) => {
              setCookAgain(value === "unsure" ? null : value === "yes");
              edit();
            }}
            options={[
              { value: "yes", label: t(msg`Yes`) },
              { value: "no", label: t(msg`No`) },
              { value: "unsure", label: t(msg`Not sure`) },
            ]}
          />
          <Field label={t(msg`Anything you'd change? (optional)`)}>
            <Textarea
              rows={4}
              maxLength={2000}
              value={comment}
              onChange={(event) => {
                setComment(event.target.value);
                edit();
              }}
            />
          </Field>
        </FieldSet>
        {error && (
          <div role="alert" className="notice mb-4">
            <p>{t(error)}</p>
            <Button
              type="button"
              variant="outline"
              className="mt-3"
              disabled={busy}
              onClick={() => void loadSaved()}
            >
              {t(msg`Load saved feedback`)}
            </Button>
          </div>
        )}
        {success && (
          <p role="status" className="text-sm mb-4">
            {t(msg`Thank you. Your feedback is saved.`)}
          </p>
        )}
        <Button type="submit" disabled={!rating || busy}>
          {busy
            ? t(msg`Saving…`)
            : saved
              ? t(msg`Update feedback`)
              : t(msg`Save feedback`)}
        </Button>
      </form>
    </Card>
  );
}

