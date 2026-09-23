import { defineConfig } from "@lingui/cli";
import { formatter } from "@lingui/format-po";
import { locales, sourceLocale } from "@gather/meal-kit/locales";

export default defineConfig({
  locales: Object.keys(locales),
  sourceLocale,
  compileNamespace: "es",
  catalogs: [
    { path: "<rootDir>/locales/{locale}/messages", include: ["<rootDir>/src", "<rootDir>/../../apps/storefront/src", "<rootDir>/../../apps/backoffice/src"] },
  ],
  format: formatter({ origins: false, lineNumbers: false }),
});
