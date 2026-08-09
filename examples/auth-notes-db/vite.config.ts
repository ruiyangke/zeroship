import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// Server-only app: no index.html / client bundle, just `src/index.ts` exporting
// RPC procedures. The plugin injects the static stub so the client build has an
// input and the .zship is emitted with the worker module + manifest.
//
// `devAuth` configures TWO dev users, which is the whole point of this example:
// per-user row scoping is only meaningful if you can sign in as somebody else
// and fail to read the first user's rows. With >1 user the dev `/authorize`
// login form renders an email dropdown.
export default defineConfig({
  plugins: [
    zeroship({
      devAuth: {
        users: [
          {
            id: "pws_alice0000000000000000",
            email: "alice@localhost",
            name: "Alice",
            password: "alice",
          },
          {
            id: "pws_bob00000000000000000",
            email: "bob@localhost",
            name: "Bob",
            password: "bob",
          },
        ],
        defaultUserId: "pws_alice0000000000000000",
      },
    }),
  ],
  // The dev runtime writes SQLite + redb files under .zeroship/; keep vite's
  // watcher off those volatile files.
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
