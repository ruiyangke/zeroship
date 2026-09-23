import { useLingui } from "@lingui/react";
import { msg,plural } from "@lingui/core/macro";
import * as api from "../api";
import { useGather } from "../state";
import { Button,ErrorState,Empty,Loading,useLoad } from "@gather/meal-kit/components/shared";
import { Card } from "@gather/meal-kit/components/ui/card";

export function RecipeFeedbackWorkspace() {
  const { market, locale } = useGather();
  const { _: t } = useLingui();
  const result = useLoad(
    async () => api.getRecipeFeedback({ market }),
    [market],
  );
  return (
    <div className="mt-6">
      <div className="flex items-center justify-between gap-4 mb-6">
        <h2>{t(msg`Recent recipe feedback`)}</h2>
        <Button variant="outline" onClick={result.refresh}>
          {t(msg`Refresh`)}
        </Button>
      </div>
      {result.error ? (
        <ErrorState error={result.error} retry={result.refresh} />
      ) : !result.data ? (
        <Loading />
      ) : !result.data.length ? (
        <Empty
          title={t(msg`No recipe feedback yet`)}
          text={t(msg`Customer feedback will appear here after delivery.`)}
        />
      ) : (
        <div className="grid gap-4 md:grid-cols-2">
          {result.data.map((feedback) => (
            <Card className="p-5" key={feedback.id}>
              <h3 className="text-xl">{feedback.recipeName[locale]}</h3>
              <p>
                {t(
                  msg({
                    message: plural(feedback.rating, {
                      one: "# star",
                      other: "# stars",
                    }),
                  }),
                )}
              </p>
              {feedback.cookAgain !== null && (
                <p className="text-sm">
                  {feedback.cookAgain
                    ? t(msg`Would cook again`)
                    : t(msg`Would not cook again`)}
                </p>
              )}
              {feedback.comment && (
                <p className="whitespace-pre-wrap break-words my-3">
                  {feedback.comment}
                </p>
              )}
              <p className="text-xs text-muted-foreground">
                <time dateTime={feedback.updatedAt}>
                  {new Intl.DateTimeFormat(locale, {
                    dateStyle: "medium",
                  }).format(new Date(feedback.updatedAt))}
                </time>{" "}
                · {feedback.orderId}
              </p>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
