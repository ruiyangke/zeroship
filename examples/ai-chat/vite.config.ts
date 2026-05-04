// AI Chat demo — exercises the AI SDK v5 useChat hook against a
// zeroship streaming RPC procedure. Same plugin as the other demos:
// `@zeroship/vite-plugin` handles client + server bundles and writes
// `dist/app.zship` for deploy.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [react(), zeroship()],
});
