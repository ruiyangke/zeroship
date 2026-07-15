import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship()],
});
