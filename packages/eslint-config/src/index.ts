/**
 * @zeroship/eslint-config — shareable config + custom rules for
 * zeroship apps. The current shipped rule is D1's `no-unindexed-query`
 * (see `./rules/no-unindexed-query.ts`).
 *
 * Consumer wiring (flat config):
 *
 * ```js
 * // eslint.config.js
 * import zeroship from "@zeroship/eslint-config";
 * export default [zeroship.recommended];
 * ```
 */
import noUnindexedQuery from "./rules/no-unindexed-query.js";

/** Map of rule name → RuleModule, exposed as an ESLint flat-config plugin. */
export const plugin = {
  rules: {
    "no-unindexed-query": noUnindexedQuery,
  },
};

/** Recommended preset: enables every rule shipped here as a warning. */
export const recommended = {
  plugins: {
    "@zeroship": plugin,
  },
  rules: {
    "@zeroship/no-unindexed-query": "warn",
  },
};

export { noUnindexedQuery };
export default { plugin, recommended, noUnindexedQuery };
