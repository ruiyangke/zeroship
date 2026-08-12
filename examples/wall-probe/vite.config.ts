import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// wall-probe is a SERVER-ONLY app (like examples/env-probe and
// examples/error-probe): no index.html, no client bundle, only `src/index.ts`
// exporting RPC procedures.
//
// Its whole job is the PER-REQUEST WALL CLOCK, which is the one runtime limit
// the two tiers do not agree on:
//
//   pnpm dev   unbounded  (crates/runtime/src/core/serve.rs, `wall_timeout: None`)
//   deployed   5s         (FREE_TIER_RUNTIME_LIMITS, crates/core/src/types.rs)
//
// A creator whose request takes longer than 5s therefore sees it WORK locally
// and 504 in production, with no local signal. That divergence is recorded in
// docs/pilot/e2e-scenarios.md under "Divergences that remain, pinned rather
// than fixed", and until this app existed nothing executed it.
//
// devServerPort is set explicitly (3098). Examples that leave the 3001 default
// collide when two run at once -- see the note on that in the repo's example
// port allocation. 3098 was checked to have zero hits tree-wide before it was
// chosen here.
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
