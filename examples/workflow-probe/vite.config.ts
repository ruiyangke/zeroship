import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// An explicit port, because the default (3001) is shared by every example that
// does not set one and two of them cannot run at once. 3051 is not
// claimed by any other example: 3041, 3011, 3021, 3081, 3061, 3013 and the
// 3001 default are taken.
const env = (globalThis as { process?: { env?: Record<string, string | undefined> } })
  .process?.env;
const devServerPort = Number(env?.WORKFLOW_PROBE_API_PORT ?? 3051);

export default defineConfig({
  plugins: [zeroship({ devServerPort })],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
