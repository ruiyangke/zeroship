import { useEffect, useId, useState } from "react";
import { Calendar } from "@gather/meal-kit/components/ui/calendar";
import { enUS, enGB, zhCN } from "react-day-picker/locale";
import { msg } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { useGather } from "../state";
import { deliveryDates, deliveryLabel } from "@gather/meal-kit/catalog";

const calendarDate = (value: string) => new Date(`${value}T12:00:00Z`);
const dateKey = (date: Date) => date.toISOString().slice(0, 10);

export function DeliveryCalendar({
  value,
  dates,
  onChange,
}: {
  value: string;
  dates: string[];
  onChange: (value: string) => void;
}) {
  const { locale, market } = useGather();
  const { _: t } = useLingui();
  const labelId = useId();
  const horizon = deliveryDates(market);
  const selected = dates.includes(value) ? calendarDate(value) : undefined;
  const [month, setMonth] = useState(
    () => selected ?? calendarDate(dates[0] ?? horizon[0]),
  );
  const selectedKey = selected ? value : undefined;
  useEffect(() => {
    if (selectedKey) setMonth(calendarDate(selectedKey));
  }, [selectedKey]);
  const dayLabel = (date: Date) =>
    new Intl.DateTimeFormat(locale, {
      year: "numeric",
      month: "long",
      day: "numeric",
      weekday: "long",
      timeZone: "UTC",
    }).format(date);
  return (
    <div className="delivery-calendar-field">
      <h2 id={labelId} className="field-legend">
        {t(msg`Delivery date`)}
      </h2>
      <p className="text-sm text-muted-foreground mb-4">
        {t(msg`Choose a highlighted day for your delivery.`)}
      </p>
      <Calendar
        className="delivery-calendar"
        mode="single"
        required
        selected={selected}
        month={month}
        onMonthChange={setMonth}
        onSelect={(date) => {
          if (date && dates.includes(dateKey(date))) onChange(dateKey(date));
        }}
        timeZone="UTC"
        locale={locale === "zh" ? zhCN : market === "us" ? enUS : enGB}
        weekStartsOn={market === "us" ? 0 : 1}
        startMonth={calendarDate(horizon[0])}
        endMonth={calendarDate(horizon.at(-1)!)}
        showOutsideDays={false}
        disabled={(date) => !dates.includes(dateKey(date))}
        modifiers={{ available: dates.map(calendarDate) }}
        modifiersClassNames={{ available: "delivery-available" }}
        aria-labelledby={labelId}
        labels={{
          labelGrid: () => t(msg`Delivery date`),
          labelNav: () => t(msg`Calendar navigation`),
          labelNext: () => t(msg`Next month`),
          labelPrevious: () => t(msg`Previous month`),
          labelDayButton: (date, modifiers) => {
            const day = dayLabel(date);
            return modifiers.disabled
              ? t(msg`${day}, unavailable`)
              : modifiers.selected
                ? t(msg`${day}, selected`)
                : t(msg`${day}, available for delivery`);
          },
        }}
        footer={
          selected
            ? t(msg`Delivery: ${deliveryLabel(value, market, locale)}`)
            : t(msg`Select a delivery day to continue.`)
        }
      />
    </div>
  );
}
