// Bench fixture vite config.
//
// Just `@zeroship/vite-plugin` — every procedure exported from
// `src/server.ts` becomes an RPC method on the spec wire
// (`/_zs/v1/<id>` with the current RPC `{ json: <input> }` envelope).
// Build outputs `dist/server/index.js` (worker bundle) + `dist/app.zship`
// (deploy artifact). The bench server (`zeroship serve`) loads the
// worker bundle directly.

import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship()],
});
