// packages/zeroship-rpc-client/src/_hooks.ts
//
// Closure-private hook registry. The whole point: keep
// `@zeroship/rpc-client` framework-agnostic. Vue / Solid / vanilla
// consumers use the same package without React Query bytes; React
// consumers populate this registry by importing
// `@zeroship/rpc-react`, which side-effect-imports React Query and
// installs the hooks here.
//
// `__makeProcedure` reads from this registry via getters. When a
// getter fires before any framework adapter has populated the slot,
// it throws a clear install hint:
//
//   "Hooks unavailable. Install @zeroship/rpc-react and mount
//    <ZeroshipProvider>."
//
// The registry is exported as a public subpath
// (`@zeroship/rpc-client/_hooks`) so the React adapter can populate
// it from a separate package without bundling React Query into the
// core client.

/**
 * Shape of the hook adapter registry. Each slot is filled by the
 * corresponding framework adapter — `@zeroship/rpc-react` populates
 * `useQuery` / `useMutation` / `useInfiniteQuery` / `useSuspenseQuery`
 * + `useStream` (custom) + `queryClient` (set by `<ZeroshipProvider>`).
 *
 * `useSubscription` lands in Phase 7 (parallel work).
 *
 * The slots are typed as `Function | undefined` rather than the precise
 * `useQuery<TData, TError>(...)` signatures because:
 *
 *   - the registry is framework-neutral (Vue's `useQuery` has a
 *     different signature than React Query's),
 *   - re-exporting React Query types here would force a hard dep, and
 *   - `__makeProcedure` only ever calls them with a single config
 *     object; no need for the precise per-hook overload set.
 *
 * Casts at the call site narrow them to the actually-installed shape.
 */
export interface HookRegistry {
  useQuery?: Function;
  useMutation?: Function;
  useInfiniteQuery?: Function;
  useSuspenseQuery?: Function;
  useStream?: Function;
  useSubscription?: Function;
  queryClient?: unknown;
  /** Set by `<ZeroshipProvider>`'s mount. False if the framework adapter
   *  imported but no provider was rendered — in that case hooks would
   *  return stale forever. The getter throws a more specific error. */
  providerMounted?: boolean;
}

/**
 * Side-effect-populated registry. Mutated by `@zeroship/rpc-react` on
 * module evaluation. The reference is module-private from the package
 * surface — but the public subpath `@zeroship/rpc-client/_hooks`
 * exports it so the React adapter (or any other framework adapter)
 * can install hooks here without static-importing
 * `@zeroship/rpc-client`'s internals.
 */
export const _hookRegistry: HookRegistry = {};

/** Install hint shown when a hook getter fires before the registry is populated. */
export const HOOK_UNAVAILABLE_MESSAGE =
  "Hooks unavailable. Install @zeroship/rpc-react and mount <ZeroshipProvider>.";
