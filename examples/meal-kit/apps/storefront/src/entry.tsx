import { Select, SelectItem } from "@gather/meal-kit/components/select-field";
import { useState } from "react";
import { Link, Navigate } from "react-router-dom";
import { msg } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { LocaleProvider } from "@gather/meal-kit/i18n";
import { markets, type MarketId } from "@gather/meal-kit/catalog";
import { locales, languageName, resolveLocale, type Locale } from "@gather/meal-kit/locales";

export function rememberMarket(market: MarketId) {
  try {
    localStorage.setItem("gather.market", market);
  } catch {}
}

export function MarketEntry({ missing = false }: { missing?: boolean }) {
  const [locale, setLocale] = useState<Locale>(() =>
    resolveLocale(navigator.language),
  );
  const [saved] = useState(() => {
    try {
      const value = localStorage.getItem("gather.market");
      return value && Object.hasOwn(markets, value) ? value : null;
    } catch {
      return null;
    }
  });
  if (saved && !missing)
    return <Navigate replace to={`/m/${saved}/${locale}`} />;
  return (
    <LocaleProvider locale={locale}>
      <EntryContent locale={locale} setLocale={setLocale} missing={missing} />
    </LocaleProvider>
  );
}
function EntryContent({
  locale,
  setLocale,
  missing,
}: {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  missing: boolean;
}) {
  const { _: t } = useLingui();
  return (
    <main className="page max-w-3xl mx-auto py-12">
      <div className="announcement">
        {t(msg`Store preview · No charges or deliveries`)}
      </div>
      <div className="flex justify-between items-center my-8">
        <span className="brand">gather</span>
        <Select
          aria-label={t(msg`Language`)}
          className="w-auto"
          value={locale}
          onValueChange={(selectedValue) => setLocale(selectedValue as Locale)}
        >
          {(Object.keys(locales) as Locale[]).map((id) => (
            <SelectItem key={id} value={id}>
              {languageName(id)}
            </SelectItem>
          ))}
        </Select>
      </div>
      <h1 className="text-4xl mb-4">
        {missing
          ? t(msg`We couldn't find that page`)
          : t(msg`Where will you be cooking?`)}
      </h1>
      <p className="text-muted-foreground mb-8">
        {missing
          ? t(msg`Choose your delivery country to return to Gather.`)
          : t(
              msg`Choose your delivery country to explore local meals and delivery options.`,
            )}
      </p>
      <nav aria-label={t(msg`Delivery country`)} className="grid gap-4">
        {Object.entries(markets).map(([id, market]) => (
          <Link
            className="panel flex justify-between items-center text-lg"
            key={id}
            to={`/m/${id}/${locale}`}
            onClick={() => rememberMarket(id as MarketId)}
          >
            <strong>{t(market.name)}</strong>
            <span className="text-sm text-muted-foreground">
              {t(market.region)}
            </span>
          </Link>
        ))}
      </nav>
    </main>
  );
}
