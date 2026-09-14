/**
 * Declare the fixed per-deployment policy used to authorize unmasking.
 *
 * ```ts
 * import { defineMaskPolicy } from "@zeroship/db";
 *
 * defineMaskPolicy({
 *   admin:   ["public", "pii", "spi", "phi", "pci", "internal"],
 *   support: ["public", "pii"],
 *   user:    ["public"],
 *   auto:    ["public", "pii", "spi", "phi", "pci", "internal"],
 * });
 * ```
 *
 * Declare it during app startup. The host validates and retains the declaration
 * until native finalization installs and seals it. Changes require a deployment.
 * When absent, only
 * the `auto` actor may unmask. A declared policy retains that grant unless it
 * explicitly restricts `auto`.
 */
import { env } from "zeroship";
import type { Classification } from "./types";

/** Actor role to allowed classifications. Missing roles cannot unmask. */
export interface MaskPolicy {
  readonly [role: string]: readonly Classification[];
}

/** Declare configuration while the host evaluates the startup entry. */
export function defineMaskPolicy(policy: MaskPolicy): void {
  const db = env.db as unknown as {
    declareMaskPolicy?: (policy: MaskPolicy) => void;
  } | undefined;
  if (typeof db?.declareMaskPolicy !== "function") {
    throw Object.assign(new Error("defineMaskPolicy requires the native database startup binding"), {
      code: "DB_STARTUP_BINDING_REQUIRED",
    });
  }
  db.declareMaskPolicy(policy);
}
