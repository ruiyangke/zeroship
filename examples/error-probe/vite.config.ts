import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// error-probe is a SERVER-ONLY app (like examples/auth-probe): no index.html,
// no client bundle - only `src/index.ts` exporting RPC procedures that throw.
//
// No `devAuth` block on purpose. Every procedure here is `auth: "anonymous"`, so no
// identity is ever involved and the only thing the two tiers can disagree about
// is the ERROR ENVELOPE. See `tests/e2e_dev_vs_deployed_errors.sh`.
export default defineConfig({
  plugins: [
    zeroship({
      devServerPort: Number(
        (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env
          ?.ERROR_PROBE_API_PORT ?? 3093,
      ),
    }),
  ],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
