/** Framework-internal dev dispatch and module normalization, pending retirement. */
export { normalizeUserModule } from "./normalize.js";
export type { NormalizedUserModule } from "./normalize.js";

export { createFetchHandler } from "./fetch-handler.js";
export type { LoadNormalized } from "./fetch-handler.js";

export { devEntry } from "./dev-entry.js";
export type { DevEntry, DevEntryOptions } from "./dev-entry.js";

// Dev auth is reached through the dev entry or its dedicated subpath.

// The dev entry imports dispatcher.js for its registration side effect.
