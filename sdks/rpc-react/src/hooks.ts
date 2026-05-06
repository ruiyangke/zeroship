//
// Side-effect-populates `_hookRegistry` from `@zeroship/rpc-client/_hooks`
// with TanStack Query's hooks. Importing `@zeroship/rpc-react` (which
// re-exports this module) is the load-bearing event — without that
// import nothing happens, and `__makeProcedure`-built handles still
// throw the install hint when their `.useQuery` getter fires.
//
// Why a side effect:
//   - keeps `@zeroship/rpc-client` framework-agnostic (Vue, Solid,
//     vanilla bundles never see React Query),
//   - lets the React Query peer dep stay in `@zeroship/rpc-react`
//     where it belongs,
//   - gives the build pipeline tree-shake leverage: client bundles
//     that never call `proc.useQuery` (e.g. workers) can drop the
//     adapter even if it's installed.

import {
  useQuery,
  useMutation,
  useInfiniteQuery,
  useSuspenseQuery,
} from "@tanstack/react-query";
import { _hookRegistry } from "@zeroship/rpc-client/_hooks";

import { useStream } from "./use-stream.js";

// ── Populate the registry on module evaluation ─────────────────────
//
// Each slot is keyed by name in `_hookRegistry`; `__makeProcedure`'s
// kind-gated getters look these up at access time. The first import
// of `@zeroship/rpc-react` (typically at the app root, where
// `<ZeroshipProvider>` mounts) is what flips the switch.

_hookRegistry.useQuery = useQuery as unknown as Function;
_hookRegistry.useMutation = useMutation as unknown as Function;
_hookRegistry.useInfiniteQuery = useInfiniteQuery as unknown as Function;
_hookRegistry.useSuspenseQuery = useSuspenseQuery as unknown as Function;

// `useStream` is our own hook (TanStack Query has experimental stream
// support but it's not stable at the time of writing).
_hookRegistry.useStream = useStream as unknown as Function;

// ── useSubscription stub ───────────────────────────────────────────
//
// We install a stub that throws UNIMPLEMENTED with a clear "land in a
// follow-up" message so procedures declared `kind: "subscription"`
// have a useful error at the point of access instead of "Hooks
// unavailable" (which would suggest a missing install).
class UnimplementedError extends Error {
  readonly code = "UNIMPLEMENTED";
  constructor() {
    super(
      "useSubscription is not implemented yet. The planned WebSocket subscription transport is documented in docs/proposals/rpc-v2.md §6.",
    );
    this.name = "UnimplementedError";
  }
}

_hookRegistry.useSubscription = (() => {
  throw new UnimplementedError();
}) as unknown as Function;
