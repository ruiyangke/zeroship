import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// wall-probe is a SERVER-ONLY app (like examples/env-probe and
// examples/error-probe): no index.html, no client bundle, only `src/index.ts`
// exporting RPC procedures.
//
// Its whole job is the PER-REQUEST WALL CLOCK, which is the one runtime limit
// the two tiers do not agree on:
//
//   pnpm dev   unbounded  (crates/zeroship-runtime/src/core/serve.rs, `wall_timeout: None`)
//   deployed   5s         (FREE_TIER_RUNTIME_LIMITS, crates/zeroship-core/src/types.rs)
//
// A creator whose request takes longer than 5s therefore sees it WORK locally
// and 504 in production, with no local signal. That divergence is deliberately
// pinned rather than fixed; this app exists to execute it.
//
// devServerPort is set explicitly (3098) so two examples running at once do
// not collide on the shared 3001 default.
export default defineConfig({
  plugins: [
    zeroship({
      devServerPort: Number(
        (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env
          ?.WALL_PROBE_API_PORT ?? 3098,
      ),
    }),
  ],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
