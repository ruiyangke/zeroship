import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  // Stage 5c — schema is read off `default.schema` of the entry by
  // the runtime bootstrap. No plugin option needed: the entry exports
  // `default = { schema, fetch, rpc }` and that's the wire contract.
  plugins: [zeroship()],
});
