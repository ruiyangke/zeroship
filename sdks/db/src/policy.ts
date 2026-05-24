/**
 * **P5.5 PR 5** — `defineMaskPolicy()` per-app mask policy declarator.
 *
 * The unmask authorization layer (PR 4 shipped the wire + audit + decrypt
 * machinery; PR 4's `check_unmask_authorization` was a default-deny stub
 * that only granted the `auto` actor) now reads a per-app policy declared
 * by the creator at bootstrap time.
 *
 * ### Usage
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
 * Call this from the app's bootstrap (the default export's bootstrap
 * hook, or at module top-level — both run before the first request
 * reaches the dispatcher). The SDK flushes the pending policy through
 * `zeroship.db.setMaskPolicy` once at app init.
 *
 * ### Default policy
 *
 * If `defineMaskPolicy()` is NEVER called, the platform falls back to
 * PR 4's strict default: only the `auto` actor kind (system writes,
 * migrations, background jobs) can unmask. Every other actor is denied.
 *
 * ### The `auto` actor fallback rule
 *
 * Even when an app DOES declare a policy, the `auto` actor retains its
 * "everything by default" grant UNLESS the policy explicitly lists
 * `auto` with a restricted classification set. Reason: system writes
 * (migrations, background jobs, the platform itself) need uniform
 * access regardless of app policy. To restrict the system actor, list
 * `auto` explicitly in the policy.
 *
 * ### Validation
 *
 * Every classification value must be one of the six built-ins listed in
 * the `Classification` taxonomy. Anything else throws
 * `invalid_mask_classification` at declare-time (and again at
 * Rust-time, belt-and-braces — see
 * `crates/plugin-db/src/crud/unmask.rs::dispatch_set_mask_policy`).
 */
import type { Classification } from "./types.js";

/**
 * **P5.5 PR 5** — actor-role → allowed classifications map.
 *
 * Keys are app-defined role strings (the actor's `kind` field on the
 * context passed to `.unmask({ actor })`). The reserved `auto` key
 * covers system actors (migrations, background jobs, the platform).
 * Values are arrays of classifications the role is permitted to
 * unmask.
 *
 * Roles missing from the map are treated as having no unmask
 * privileges — any classification they request is denied.
 */
export interface MaskPolicy {
  readonly [role: string]: readonly Classification[];
}

/**
 * The six canonical classifications. Mirrors `Classification` in
 * `./types.ts` and `crate::diff::Classification` on the Rust side.
 *
 * Pinned here as a runtime-checkable array (not just a TS type) so
 * `defineMaskPolicy` can validate user-declared classifications
 * structurally.
 */
const VALID_CLASSIFICATIONS: readonly Classification[] = [
  "public",
  "pii",
  "spi",
  "phi",
  "pci",
  "internal",
];

/**
 * Holding slot for the policy declared by `defineMaskPolicy()` — the
 * SDK bootstrap (`@zeroship/bootstrap`) reads this once at app init
 * via `_flushPendingMaskPolicy()` and flushes through the native op.
 *
 * Module-local — never exposed. Re-declaring policy in the same
 * process overwrites: the platform's mask policy is single-shot at
 * boot. A second call after `_flushPendingMaskPolicy()` returned the
 * previous one is honored on the next flush; calls after the first
 * request reaches the dispatcher have no effect (no second flush).
 */
let _pendingPolicy: MaskPolicy | null = null;

/**
 * **P5.5 PR 5** — declare the per-app mask policy. See module-level
 * doc-comment for usage examples.
 *
 * @throws `invalid_mask_classification` when any classification value
 *   is not one of the six built-ins.
 */
export function defineMaskPolicy(policy: MaskPolicy): void {
  if (policy === null || typeof policy !== "object") {
    throw Object.assign(
      new Error(
        "defineMaskPolicy: policy must be an object mapping role strings " +
          "to arrays of classifications",
      ),
      { code: "invalid_mask_policy_shape" as const },
    );
  }
  for (const [role, classifications] of Object.entries(policy)) {
    if (!Array.isArray(classifications)) {
      throw Object.assign(
        new Error(
          `defineMaskPolicy: role "${role}" must map to an array of ` +
            `classifications, got ${typeof classifications}`,
        ),
        { code: "invalid_mask_policy_shape" as const },
      );
    }
    for (const c of classifications) {
      if (typeof c !== "string" || !VALID_CLASSIFICATIONS.includes(c as Classification)) {
        throw Object.assign(
          new Error(
            `defineMaskPolicy: role "${role}" includes invalid classification ` +
              `"${String(c)}". Valid: ${VALID_CLASSIFICATIONS.join(", ")}.`,
          ),
          { code: "invalid_mask_classification" as const },
        );
      }
    }
  }
  // Shallow-clone so a later mutation of the caller's object doesn't
  // bleed into our stored copy (the policy is meant to be effectively
  // immutable from the platform's perspective once flushed).
  const cloned: { [role: string]: readonly Classification[] } = {};
  for (const [role, classifications] of Object.entries(policy)) {
    cloned[role] = [...classifications];
  }
  _pendingPolicy = cloned;
}

/**
 * **Framework-internal** — drain the pending policy slot. The
 * `@zeroship/bootstrap` runtime-entry calls this once during app
 * init; the returned policy (when non-null) is flushed through the
 * `zeroship.db.setMaskPolicy` native op so the Rust side cache + the
 * per-app storage layer pick it up.
 *
 * Returns `null` when no policy has been declared — the platform
 * then keeps PR 4's default-deny stub.
 *
 * @internal — do NOT call from user code. The bootstrap is the only
 *   consumer; calling from app code would race with the bootstrap's
 *   own flush and leave the policy unflushed.
 */
export function _flushPendingMaskPolicy(): MaskPolicy | null {
  const p = _pendingPolicy;
  _pendingPolicy = null;
  return p;
}

/**
 * **Test-only** — inspect the pending slot without draining it. Used
 * by the SDK unit tests to verify `defineMaskPolicy` parked the right
 * shape; production code never reads the pending slot directly.
 *
 * @internal
 */
export function _peekPendingMaskPolicy(): MaskPolicy | null {
  return _pendingPolicy;
}
