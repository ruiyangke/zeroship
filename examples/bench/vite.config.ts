// Bench fixture vite config.
//
// Just `@zeroship/vite-plugin` — every procedure exported from
// `src/server.ts` becomes an RPC method on the spec wire
// (`POST /_zs/v1/<id>` with superjson `{ json: <input> }` envelope).
// Build outputs `dist/server/index.js` (worker bundle) + `dist/app.zsapp`
// (deploy artifact). The bench server (`zeroship serve`) loads the
// worker bundle directly.

import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship()],
});
