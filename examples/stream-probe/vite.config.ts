import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

const env = (globalThis as { process?: { env?: Record<string, string | undefined> } })
  .process?.env;
const devServerPort = Number(env?.STREAM_PROBE_API_PORT ?? 3061);

export default defineConfig({
  plugins: [zeroship({ devServerPort })],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
