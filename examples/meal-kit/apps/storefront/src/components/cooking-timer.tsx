import { useState, useSyncExternalStore } from "react";
import { Link } from "react-router-dom";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { Clock3, Pause, Play, RotateCcw, X } from "lucide-react";
import { cookingTimers, type TimerContext } from "../cooking-timers";
import { useGather } from "../state";
import { Button, Field, Input } from "@gather/meal-kit/components/shared";
import { Alert, AlertTitle, AlertDescription } from "@gather/meal-kit/components/ui/alert";
import {
  Collapsible,
  CollapsibleTrigger,
  CollapsibleContent,
} from "@gather/meal-kit/components/ui/collapsible";

const useTimers = () =>
  useSyncExternalStore(cookingTimers.subscribe, cookingTimers.getSnapshot);
function timeLabel(milliseconds: number, locale: string) {
  const seconds = Math.ceil(milliseconds / 1000);
  const format = new Intl.NumberFormat(locale, {
    minimumIntegerDigits: 2,
    useGrouping: false,
  });
  return `${format.format(Math.floor(seconds / 60))}:${format.format(seconds % 60)}`;
}

export function StepTimer({ context }: { context: TimerContext }) {
  const timers = useTimers();
  const timer = timers.find((timer) => timer.key === context.key);
  const { _: t } = useLingui();
  const { locale } = useGather();
  const [open, setOpen] = useState(false);
  const [minutes, setMinutes] = useState("5");
  const [seconds, setSeconds] = useState("0");
  const duration = (Number(minutes) * 60 + Number(seconds)) * 1000;
  const valid =
    /^\d+$/.test(minutes) &&
    /^\d+$/.test(seconds) &&
    Number(seconds) < 60 &&
    duration >= 1000 &&
    duration <= 180 * 60_000;
  const step = context.step;
  return (
    <div
      className="no-print mt-4"
      role="group"
      aria-label={t(msg`Timer for step ${step}`)}
    >
      {timer ? (
        <div className="flex flex-wrap items-center gap-3 rounded-lg border bg-muted/40 p-3">
          <Clock3 aria-hidden="true" className="size-4" />
          <span
            role="timer"
            aria-live="off"
            className="font-mono text-xl tabular-nums"
            aria-label={t(msg`Time remaining`)}
          >
            {timeLabel(timer.remaining, locale)}
          </span>
          {timer.status === "finished" ? (
            <span>{t(msg`Timer finished`)}</span>
          ) : (
            <Button
              size="sm"
              variant="outline"
              onClick={() =>
                timer.status === "running"
                  ? cookingTimers.pause(timer.key)
                  : cookingTimers.resume(timer.key)
              }
            >
              {timer.status === "running" ? <Pause /> : <Play />}
              {timer.status === "running"
                ? t(msg`Pause timer`)
                : t(msg`Resume timer`)}
            </Button>
          )}
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              setMinutes(String(Math.floor(timer.duration / 60_000)));
              setSeconds(String((timer.duration / 1000) % 60));
              cookingTimers.remove(timer.key);
              setOpen(true);
            }}
          >
            <RotateCcw />
            {t(msg`Reset timer`)}
          </Button>
        </div>
      ) : (
        <Collapsible open={open} onOpenChange={setOpen}>
          <CollapsibleTrigger render={<Button variant="outline" size="sm" />}>
            <Clock3 />
            {t(msg`Add timer`)}
          </CollapsibleTrigger>
          <CollapsibleContent>
            <div className="grid grid-cols-2 gap-3 mt-4 max-w-sm">
              <Field label={t(msg`Minutes`)}>
                <Input
                  type="number"
                  min={0}
                  max={180}
                  step={1}
                  inputMode="numeric"
                  value={minutes}
                  onChange={(event) => setMinutes(event.target.value)}
                />
              </Field>
              <Field label={t(msg`Seconds`)}>
                <Input
                  type="number"
                  min={0}
                  max={59}
                  step={1}
                  inputMode="numeric"
                  value={seconds}
                  onChange={(event) => setSeconds(event.target.value)}
                />
              </Field>
            </div>
            <Button
              size="sm"
              disabled={!valid}
              onClick={() => cookingTimers.start(context, duration)}
            >
              <Play />
              {t(msg`Start timer`)}
            </Button>
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}

export function KitchenTimers() {
  const timers = useTimers();
  const { _: t } = useLingui();
  const { locale } = useGather();
  if (!timers.length) return null;
  return (
    <Alert
      className="mt-5 no-print"
      role="region"
      aria-label={t(msg`Kitchen timers`)}
    >
      <Clock3 aria-hidden="true" />
      <AlertTitle>{t(msg`Kitchen timers`)}</AlertTitle>
      <AlertDescription>
        <p className="mb-3">{t(msg`Keep Gather open to see timer alerts.`)}</p>
        <ul className="w-full space-y-3">
          {timers.map((timer) => {
            const step = timer.step;
            return (
              <li key={timer.key} className="flex flex-wrap items-center gap-3">
                <Link
                  className="underline flex-1 min-w-0"
                  to={`/m/${timer.market}/${locale}/${timer.orderId ? `orders/${timer.orderId}/cook` : "recipes"}/${timer.recipeId}#step-${step}`}
                >
                  {timer.name[locale]} · {t(msg`Step ${step}`)}
                </Link>
                {timer.status === "finished" ? (
                  <span role="status" className="font-semibold">
                    {t(msg`Timer finished`)}
                  </span>
                ) : (
                  <span className="tabular-nums" aria-live="off">
                    {timer.status === "paused"
                      ? t(msg`Paused`)
                      : timeLabel(timer.remaining, locale)}
                  </span>
                )}
                <Button
                  variant="ghost"
                  size="icon"
                  aria-label={t(msg`Dismiss timer for step ${step}`)}
                  onClick={() => cookingTimers.remove(timer.key)}
                >
                  <X />
                </Button>
              </li>
            );
          })}
        </ul>
      </AlertDescription>
    </Alert>
  );
}
