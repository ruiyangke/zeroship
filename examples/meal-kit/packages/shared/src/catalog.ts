import { countryPolicies } from "@gather/meal-kit/countries";
import { locales, type Locale } from "@gather/meal-kit/locales";
export type { Locale } from "@gather/meal-kit/locales";
export type MarketId = "us" | "uk" | "cn";
export const allergenLabels: Record<string, string> = {
  milk: /* i18n */ "Milk",
  wheat: /* i18n */ "Wheat",
  nuts: /* i18n */ "Nuts",
  fish: /* i18n */ "Fish",
  soy: /* i18n */ "Soy",
  sesame: /* i18n */ "Sesame",
};
export const markets = {
  us: {
    id: "us",
    name: /* i18n */ "United States",
    currency: "USD",
    timezone: "America/New_York",
    region: /* i18n */ "New York",
    price: 1099,
    shipping: 699,
    postal: "10001",
    country: "US",
  },
  uk: {
    id: "uk",
    name: /* i18n */ "United Kingdom",
    currency: "GBP",
    timezone: "Europe/London",
    region: /* i18n */ "London",
    price: 649,
    shipping: 499,
    postal: "SW1A 1AA",
    country: "GB",
  },
  cn: {
    id: "cn",
    name: /* i18n */ "China",
    currency: "CNY",
    timezone: "Asia/Shanghai",
    region: /* i18n */ "Shanghai",
    price: 3900,
    shipping: 1200,
    postal: "",
    country: "CN",
  },
} as const;
export type { Recipe } from "@gather/meal-kit/catalog-domain";
export function money(amount: number, market: MarketId, locale: Locale = "en") {
  return new Intl.NumberFormat(formatLocale(locale, market), {
    style: "currency",
    currency: markets[market].currency,
  }).format(amount / 100);
}
export function localDate(now: number, timezone: string) {
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: timezone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).formatToParts(now);
  const value = (part: string) =>
    parts.find((entry) => entry.type === part)!.value;
  return [value("year"), value("month"), value("day")].join("-");
}
export function cutoffForDate(market: MarketId, date: string) {
  const policy = countryPolicies[market];
  const day = Date.parse(date + "T00:00:00Z") - policy.cutoffDays * 86_400_000;
  const target = day + policy.cutoffHour * 3_600_000;
  const formatter = new Intl.DateTimeFormat("en-CA", {
    timeZone: markets[market].timezone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  });
  let instant = target;
  for (let attempt = 0; attempt < 4; attempt++) {
    const parts = formatter.formatToParts(instant);
    const part = (type: string) =>
      Number(parts.find((entry) => entry.type === type)!.value);
    const represented = Date.UTC(
      part("year"),
      part("month") - 1,
      part("day"),
      part("hour"),
      part("minute"),
      part("second"),
    );
    if (represented === target) return new Date(instant).toISOString();
    instant += target - represented;
  }
  throw new Error("The configured local cutoff cannot be resolved.");
}
export function deliveryDates(market: MarketId, now = Date.now()) {
  const start = Date.parse(
    localDate(now, markets[market].timezone) + "T12:00:00Z",
  );
  const policy = countryPolicies[market];
  return Array.from(
    { length: 28 },
    (_, offset) => new Date(start + offset * 86_400_000),
  )
    .filter((date) => policy.weekdays.includes(date.getUTCDay() as never))
    .map((date) => date.toISOString().slice(0, 10))
    .filter(
      (date) =>
        !policy.holidays.includes(date) &&
        Date.parse(cutoffForDate(market, date)) > now,
    );
}
export function deliveryLabel(date: string, market: MarketId, locale: Locale) {
  return new Intl.DateTimeFormat(formatLocale(locale, market), {
    weekday: "short",
    month: "short",
    day: "numeric",
    timeZone: markets[market].timezone,
  }).format(new Date(`${date}T12:00:00Z`));
}

export function formatLocale(locale: Locale, market: MarketId) {
  return new Intl.Locale(locales[locale].tag, {
    region: markets[market].country,
  }).toString();
}
