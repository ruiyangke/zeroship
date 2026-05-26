import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

const env = (globalThis as { process?: { env?: Record<string, string | undefined> } })
  .process?.env;
const devServerPort = Number(env?.KV_DASHBOARD_API_PORT ?? 3011);

export default defineConfig({
  plugins: [react(), zeroship({ devServerPort })],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
