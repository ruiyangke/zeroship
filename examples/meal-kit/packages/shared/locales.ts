export const locales = {
  en: { tag: "en", direction: "ltr" },
  zh: { tag: "zh-CN", direction: "ltr" },
} as const;

export type Locale = keyof typeof locales;
export const sourceLocale: Locale = "en";

export function resolveLocale(requested?: string): Locale {
  const normalized = requested?.toLowerCase().replaceAll("_", "-");
  const exact = Object.keys(locales).find(
    (key) =>
      key === normalized ||
      locales[key as Locale].tag.toLowerCase() === normalized,
  );
  if (exact) return exact as Locale;
  const language = normalized?.split("-")[0];
  return language && Object.hasOwn(locales, language)
    ? (language as Locale)
    : sourceLocale;
}

export function languageName(locale: Locale) {
  const tag = locales[locale].tag;
  return new Intl.DisplayNames([tag], { type: "language" }).of(locale) ?? tag;
}
