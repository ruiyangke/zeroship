import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// Server-only app: no index.html, no client bundle - just `src/index.ts`
// exporting RPC procedures. The plugin injects the static stub so the client
// build has an input and the .zship is emitted with the worker module.
//
// TWO dev users, which is the point of this example: "user A cannot reach
// user B's object" is only a claim you can test if you can actually sign in
// as B. With more than one user the dev `/authorize` login form renders an
// email dropdown instead of a single fixed identity.
const env = (globalThis as { process?: { env?: Record<string, string | undefined> } })
  .process?.env;

// Distinct ports so this example can run alongside the other examples' dev
// servers (kv-dashboard is on 3011, storage-gallery on 3021).
const devServerPort = Number(env?.AUTH_UPLOADS_KV_API_PORT ?? 3041);
const vitePort = Number(env?.AUTH_UPLOADS_KV_PORT ?? 5183);

export default defineConfig({
  plugins: [
    zeroship({
      devServerPort,
      devAuth: {
        users: [
          {
            id: "pws_alice000000000000000",
            email: "alice@localhost",
            name: "Alice",
          },
          {
            id: "pws_bob00000000000000000",
            email: "bob@localhost",
            name: "Bob",
          },
        ],
        defaultUserId: "pws_alice000000000000000",
      },
    }),
  ],
  // The dev runtime writes redb (kv) + LocalFs (storage) state under
  // .zeroship/; keep vite's watcher off those volatile files.
  server: { port: vitePort, strictPort: true, watch: { ignored: ["**/.zeroship/**"] } },
});
