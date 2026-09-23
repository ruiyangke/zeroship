import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import babel from "@rolldown/plugin-babel";
import { lingui, linguiTransformerBabelPreset } from "@lingui/vite-plugin";
import tailwindcss from "@tailwindcss/vite";
import { zeroship } from "@zeroship/vite-plugin";
import { fileURLToPath } from "node:url";
import {
  useWorkspaceEnvironment,
  workspaceConfigPath,
} from "../../zeroship.workspace";

// Before the plugin reads anything: the spawned runtime inherits these.
useWorkspaceEnvironment();

export default defineConfig({
  publicDir: fileURLToPath(new URL("../../packages/shared/public", import.meta.url)),
  plugins: [
    react(),
    lingui({ configPath: fileURLToPath(new URL("../../packages/shared/lingui.config.ts", import.meta.url)) }),
    babel({ presets: [linguiTransformerBabelPreset()] }),
    tailwindcss(),
    zeroship({
      // The workspace declares two apps, so this build has to say which it is.
      configPath: workspaceConfigPath,
      app: "storefront",
      // `build.dist` is the directory the packer walks; Vite fills this app's
      // own `dist`, and the two spellings have to name one directory.
      config: (c) => ({
        build: { ...c.build, dist: fileURLToPath(new URL("./dist", import.meta.url)) },
      }),
      devServerPort: Number(process.env.GATHER_API_PORT ?? 3097),
      devAuth: {
        users: [
          {
            id: "pws_gathercustomer000001",
            email: "alex@gather.example",
            name: "Alex Morgan",
          },
          {
            id: "pws_gathercustomer000002",
            email: "sam@gather.example",
            name: "Sam Chen",
          },
          {
            id: "pws_gatheroperator000001",
            email: "ops@gather.example",
            name: "Gather operations",
          },
        ],
        defaultUserId: "pws_gathercustomer000001",
      },
    }),
  ],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: {
    host: "127.0.0.1",
    port: 5197,
    strictPort: true,
    watch: { ignored: ["**/.zeroship/**"] },
  },
  build: { manifest: true },
});
