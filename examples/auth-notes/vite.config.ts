import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// auth-notes is a SERVER-ONLY app (ISS-59): there is no index.html / client
// bundle — only `src/index.ts` exporting RPC procedures. The vite-plugin
// injects the static stub so the client build has an input and the .zship
// is emitted with the worker module + manifest.
const env = (globalThis as { process?: { env?: Record<string, string | undefined> } })
  .process?.env;
const devServerPort = Number(env?.AUTH_NOTES_API_PORT ?? 3013);

export default defineConfig({
  plugins: [zeroship({ devServerPort })],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
