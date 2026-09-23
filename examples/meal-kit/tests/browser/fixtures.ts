import { testOrigin } from "../fixture/settings";
import { test as base, expect } from "@playwright/test";
import { signIn, rpc } from "./helpers";
export { expect };
export const test = base.extend<{}, { catalogReady: void }>({
  catalogReady: [
    async ({ browser }, use) => {
      const context = await browser.newContext({
        baseURL: testOrigin,
      });
      try {
        const page = await context.newPage();
        await signIn(page, "ops@gather.example");
        for (const market of ["us", "uk", "cn"])
          await rpc(context, "loadSampleMenus", { market });
      } finally {
        await context.close();
      }
      await use();
    },
    { scope: "worker", auto: true },
  ],
});
