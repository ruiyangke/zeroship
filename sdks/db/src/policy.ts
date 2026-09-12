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
 * Declare it during app startup. The bootstrap installs it in memory and seals
 * the declaration; changing it requires a new deployment. When absent, only
 * the `auto` actor may unmask. A declared policy retains that grant unless it
 * explicitly restricts `auto`.
 */
import type { Classification } from "./types";

/** Actor role to allowed classifications. Missing roles cannot unmask. */
export interface MaskPolicy {
  readonly [role: string]: readonly Classification[];
}

/** Runtime values accepted by the classification type. */
const VALID_CLASSIFICATIONS: readonly Classification[] = [
  "public",
  "pii",
  "spi",
  "phi",
  "pci",
  "internal",
];

const MASK_POLICY_STATE = Symbol.for("@zeroship/db/MaskPolicyState");

type MaskPolicyState = {
  pendingPolicy: MaskPolicy | null;
  sealed: boolean;
};

function policyState(): MaskPolicyState {
  const global = globalThis as unknown as Record<PropertyKey, MaskPolicyState | undefined>;
  return (global[MASK_POLICY_STATE] ??= { pendingPolicy: null, sealed: false });
}

/**
 * Declare the per-app mask policy.
 *
 * @throws `MASK_POLICY_IMMUTABLE` after the startup flush.
 * @throws `INVALID_MASK_CLASSIFICATION` when any classification value
 *   is unsupported.
 */
export function defineMaskPolicy(policy: MaskPolicy): void {
  const state = policyState();
  if (state.sealed) {
    throw Object.assign(
      new Error("defineMaskPolicy: policy is fixed after startup; edit the app and redeploy"),
      { code: "MASK_POLICY_IMMUTABLE" as const },
    );
  }
  if (policy === null || typeof policy !== "object") {
    throw Object.assign(
      new Error(
        "defineMaskPolicy: policy must be an object mapping role strings " +
          "to arrays of classifications",
      ),
      { code: "INVALID_MASK_POLICY_SHAPE" as const },
    );
  }
  for (const [role, classifications] of Object.entries(policy)) {
    if (!Array.isArray(classifications)) {
      throw Object.assign(
        new Error(
          `defineMaskPolicy: role "${role}" must map to an array of ` +
            `classifications, got ${typeof classifications}`,
        ),
        { code: "INVALID_MASK_POLICY_SHAPE" as const },
      );
    }
    for (const c of classifications) {
      if (typeof c !== "string" || !VALID_CLASSIFICATIONS.includes(c as Classification)) {
        throw Object.assign(
          new Error(
            `defineMaskPolicy: role "${role}" includes invalid classification ` +
              `"${String(c)}". Valid: ${VALID_CLASSIFICATIONS.join(", ")}.`,
          ),
          { code: "INVALID_MASK_CLASSIFICATION" as const },
        );
      }
    }
  }
  // Snapshot and freeze the declaration, including each classification array.
  const cloned: { [role: string]: readonly Classification[] } = Object.create(null);
  for (const [role, classifications] of Object.entries(policy)) {
    cloned[role] = Object.freeze([...classifications]);
  }
  state.pendingPolicy = Object.freeze(cloned);
}

/** Drain and seal the startup policy for the bootstrap package. @internal */
export function _flushPendingMaskPolicy(): MaskPolicy | null {
  const state = policyState();
  const p = state.pendingPolicy;
  state.pendingPolicy = null;
  state.sealed = true;
  return p;
}

/** Inspect the pending startup policy without draining it. @internal */
export function _peekPendingMaskPolicy(): MaskPolicy | null {
  return policyState().pendingPolicy;
}
