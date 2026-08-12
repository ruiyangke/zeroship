"use server";

// wall-probe - the PER-REQUEST WALL CLOCK leg of the dev-vs-deployed seam
// comparison.
//
// Sibling of examples/env-probe and examples/error-probe: a deliberately boring
// app whose only job is to let a harness run ONE identical procedure against
// `pnpm dev` and against the same `.zship` deployed behind the gateway, and
// compare the RESULTS.
//
// THE QUESTION. A creator writes a handler that takes longer than the deployed
// wall budget. What do the two tiers do?
//
//   pnpm dev   unbounded  - crates/runtime/src/core/serve.rs sets
//                           `wall_timeout: None`, so the request completes.
//   deployed   5s         - the app inherits FREE_TIER_RUNTIME_LIMITS
//                           (crates/core/src/types.rs: 5s wall, 50ms CPU), and
//                           crates/worker/src/handler.rs answers
//                           `make_error_msg(504, "request timed out")`.
//
// So the same code passes locally and 504s in production, and dev gives the
// creator no signal at all. That is the trap this app exists to pin.
//
// WHY BOTH ARMS MUST BE ASSERTED. A harness that only checked the deployed 504
// would stay green if dev ever gained a wall bound of its own - and dev gaining
// one would CLOSE the trap, which is a change worth noticing. The finding is
// the DISAGREEMENT, so both sides are reported.
//
// WHY `fast` EXISTS. It is the one-variable control: same app, same tiers, same
// dispatch path, same envelope shape, differing only in duration. If `slow`
// diverges and `fast` agrees, the divergence is the wall clock and not the
// app, the deploy, the gateway or the harness. If BOTH diverge, something more
// basic is broken and the slow result says nothing about wall budgets.
import { query } from "@zeroship/rpc/server";

// Comfortably over the deployed 5s budget and comfortably under any harness
// read timeout. 6s is a JUDGEMENT: 1s of headroom over the budget, chosen so a
// loaded host does not turn a deliberate overrun into a coin flip. It is not a
// measurement of anything.
const SLOW_MS = 6000;

export const fast = query(
  async () => {
    const started = Date.now();
    return { arm: "fast", requestedMs: 0, elapsedMs: Date.now() - started };
  },
  { id: "wallp.fast" },
);

export const slow = query(
  async () => {
    const started = Date.now();
    await new Promise((resolve) => setTimeout(resolve, SLOW_MS));
    // Reached only on a tier that does NOT bound the request. The deployed tier
    // is expected to answer 504 before this line runs, so its body comes from
    // the worker rather than from here - which is exactly the divergence.
    return { arm: "slow", requestedMs: SLOW_MS, elapsedMs: Date.now() - started };
  },
  { id: "wallp.slow" },
);
