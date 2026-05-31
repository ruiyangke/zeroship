// Vitest config for the zeroship-builder server unit tests.
//
// Deliberately does NOT load `@zeroship/vite-plugin`: that plugin rewrites
// every `"use server"` module into RPC stubs and injects the
// `virtual:zeroship-runtime` module (which references the runtime-only
// `__zs_env` global). Under unit test we want to import the server modules
// directly and mock their platform dependencies (`zeroship`,
// `@zeroship/kv`, the sandbox files API via `fetch`) with `vi.mock` /
// `vi.stubGlobal`. So this config stays minimal and lets each test stub
// what it needs.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
    // Server-unit tests (projects/env/logs, sandbox, the control-import
    // guard) run here; the e2e Playwright specs run via `test:e2e`.
    exclude: ["e2e/**", "node_modules/**", "dist/**"],
  },
});
