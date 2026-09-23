import { Button } from "@gather/meal-kit/components/ui/button";
import { setupI18n, type Messages } from "@lingui/core";
import { I18nProvider } from "@lingui/react";
import { Trans } from "@lingui/react/macro";
import { useEffect, useState, type ReactNode } from "react";
import { locales, sourceLocale, type Locale } from "@gather/meal-kit/locales";
import { messages } from "@gather/meal-kit/locales/en/messages.po";

const catalogs = import.meta.glob<{ messages: Messages }>([
  "../locales/*/messages.po",
  "!../locales/en/messages.po",
]);

export function LocaleProvider({
  locale,
  children,
}: {
  locale: Locale;
  children: ReactNode;
}) {
  const [i18n] = useState(() =>
    setupI18n({ locale: sourceLocale, messages: { [sourceLocale]: messages } }),
  );
  const [initialized, setInitialized] = useState(locale === sourceLocale);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let active = true;
    setFailed(false);
    async function activate() {
      const catalog =
        locale === sourceLocale
          ? { messages }
          : await catalogs[`../locales/${locale}/messages.po`]();
      if (!active) return;
      i18n.loadAndActivate({ locale, messages: catalog.messages });
      document.documentElement.lang = locales[locale].tag;
      document.documentElement.dir = locales[locale].direction;
      setInitialized(true);
    }
    activate().catch(() => {
      if (active) setFailed(true);
    });
    return () => {
      active = false;
    };
  }, [i18n, locale]);

  return (
    <I18nProvider i18n={i18n}>
      {failed && (
        <div role="alert" className="error-panel">
          <Trans>We couldn't load this language. Please try again.</Trans>
          <Button
            variant="ghost"
            type="button"
            className="cta"
            onClick={() => window.location.reload()}
          >
            <Trans>Reload and retry</Trans>
          </Button>
        </div>
      )}
      {initialized
        ? children
        : !failed && (
            <div role="status" className="loading">
              <Trans>Loading…</Trans>
            </div>
          )}
    </I18nProvider>
  );
}
