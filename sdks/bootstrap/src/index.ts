/**
 * `@zeroship/bootstrap` — framework-internal coordination package.
 *
 * User code MUST NOT import this. Consumers:
 *   - The runtime crate (`crates/runtime`), via `include_str!` of
 *     `dist/runtime-entry.js` and `dist/dispatcher.js`.
 *   - The Vite plugin (`@zeroship/vite-plugin`), via `import` from
 *     `src/dev-bootstrap/index.ts`.
 *
 * This barrel re-exports the surface both consumers need. Subpath
 * exports (`./install-schema`, `./dispatcher`, etc.) are documented
 * in `package.json` and used by the runtime crate's stub-points and
 * the dev-bootstrap's targeted imports.
 */
export {
  installSchema,
  model,
  normalizeSchema,
  expandUnionToFlatColumns,
  validateRefTargets,
} from "./install-schema.js";
export type {
  NormalizedSchema,
  SchemaInput,
  ValidateSchemaShape,
  TxCollection,
  TxQuery,
  TransactionOptions,
  Collections,
  DbExtensions,
  Db,
  InstallSchemaOptions,
} from "./install-schema.js";

export { normalizeUserModule } from "./normalize.js";
export type { NormalizedUserModule } from "./normalize.js";

export { createFetchHandler } from "./fetch-handler.js";
export type { LoadNormalized } from "./fetch-handler.js";

export { devEntry } from "./dev-entry.js";
export type { DevEntry, DevEntryOptions } from "./dev-entry.js";

// Side-effect-only modules (dispatcher.js installs `__zsDispatch` on
// the global; runtime-entry.js is the prod-mode TLA orchestrator).
// These are NOT re-exported from the barrel because they're used
// directly via subpath imports by the runtime crate's `include_str!`
// and the dev-entry's side-effect import.
