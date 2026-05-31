// Vitest config for the zeroship-builder server unit tests.
//
// Deliberately does NOT load `@zeroship/vite-plugin`: that plugin rewrites
// every `"use server"` module into RPC stubs and injects the
// `virtual:zeroship-runtime` module (which references the runtime-only
// `__zs_env` global). Under unit test we want to import the server modules
// directly and mock their platform dependencies (`zeroship`,
// `@zeroship/control`, `@zeroship/kv`) with `vi.mock`. So this config
// stays minimal and lets each test stub what it needs.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
    // The bespoke-login units (oauth/session) and the new control-client
    // tests run here; the e2e Playwright specs run via `test:e2e`.
    exclude: ["e2e/**", "node_modules/**", "dist/**"],
  },
});
