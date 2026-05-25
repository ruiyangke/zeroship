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

/**
 * Composite env exposed to user handlers. The interface is exported
 * (not a structural literal) so user projects can augment it via
 * `@zeroship/types`'s `zeroship-schema.d.ts` — the augmentation lifts
 * `env.db` from the bare native handle to a typed `Db<schema>`.
 */
export interface Env {
  // Intentionally permissive — plugins attach namespaces and apps add
  // scalar secrets/vars; augmentations narrow specific keys.
  [key: string]: unknown;
}

// Intentionally mutable for test injection — see index.js for rationale.
export const env: Env;
export function waitUntil(promise: Promise<unknown>): void;
export function getRequest(): Request;
export function runQuery<TIn, TOut>(
  fn: (input: TIn) => Promise<TOut> | TOut,
  input: TIn,
): Promise<TOut>;
export function runMutation<TIn, TOut>(
  fn: (input: TIn) => Promise<TOut> | TOut,
  input: TIn,
): Promise<TOut>;
export function currentUser(): unknown | null;
export function currentRequestId(): string;
export function currentTraceId(): string;
export function currentSignal(): AbortSignal;
export function currentHeaders(): Headers;
export function currentIdempotencyKey(): string | undefined;
