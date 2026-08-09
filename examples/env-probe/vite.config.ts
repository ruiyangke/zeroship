import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// env-probe is a SERVER-ONLY app (like examples/error-probe): no index.html, no
// client bundle - only `src/index.ts` exporting RPC procedures that report what
// app code can observe of the process environment.
//
// No `devAuth` block on purpose. Every procedure is `auth: "anon"`, so identity
// never enters the picture and the only thing the two tiers can disagree about
// is the ENVIRONMENT SURFACE. See `tests/e2e_dev_vs_deployed_env.sh`.
export default defineConfig({
  plugins: [
    zeroship({
      devServerPort: Number(
        (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env
          ?.ENV_PROBE_API_PORT ?? 3097,
      ),
    }),
  ],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
