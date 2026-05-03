//
// Public surface of `@zeroship/rpc-react`:
//
//   - <ZeroshipProvider client={qc}> — wraps QueryClientProvider and
//     stashes `qc` on the rpc-client hook registry.
//   - rpcInvalidate("todos.") — prefix-based bulk invalidation across
//     procedures defined under that namespace.
//   - useStream({ key, stream }) — collect AsyncIterable yields into a
//     `chunks: T[]` state.
//
// IMPORTANT: importing this package has a SIDE EFFECT. The act of
// importing wires React Query's hooks into `_hookRegistry` (declared
// in @zeroship/rpc-client/_hooks). Procedures built via
// `__makeProcedure` resolve their `.useQuery` / `.useMutation` etc.
// getters from that registry — without this side-effect import, those
// getters throw the "install hint" error.
//
// Convention: import @zeroship/rpc-react at the app root (where you
// mount <ZeroshipProvider>) and you're set. Tree-shaking still works
// for client bundles that never call the hooks because the static
// references stay flat.

// Side-effect imports — populate `_hookRegistry` on module evaluation.
import "./hooks.js";

export { ZeroshipProvider } from "./provider.js";
export type { ZeroshipProviderProps } from "./provider.js";

export { rpcInvalidate } from "./invalidate.js";

export { useStream } from "./use-stream.js";
export type { UseStreamConfig, UseStreamResult } from "./use-stream.js";
