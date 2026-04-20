/**
 * Type-level mirror of the stub's runtime shape.
 *
 * Exists so `import { env } from "zeroship"` inside SDK source files
 * type-checks when `@zeroship/types` isn't pre-loaded (e.g. in a
 * fresh test run). The SDK's own tsconfig includes `@zeroship/types`
 * which ships a richer `declare module "zeroship"` — the two are
 * structurally compatible (both declare the same export names), so
 * TypeScript's declaration merging keeps the richer types available
 * when they matter.
 */

// Intentionally mutable for test injection — see index.js for rationale.
export const env: { [key: string]: unknown };
export function waitUntil(promise: Promise<unknown>): void;
export function getRequest(): Request;
