import type { TestRunnerConfig } from "@storybook/test-runner";
import { getStoryContext } from "@storybook/test-runner";
import { checkA11y, configureAxe, injectAxe } from "axe-playwright";

/* The Test Runner boots a real Chromium against a built or running
 * Storybook and visits every story. Each visit becomes a Jest test
 * (smoke render + any `play()`). This config layers axe-core on top
 * so every story is also an a11y assertion — no separate runner.
 *
 * Per-story opt-outs go through `parameters.a11y` on the story or
 * meta (`disable: true` to skip, `config.rules` to tune axe, etc.) so
 * we never edit this file to silence a check. */
const config: TestRunnerConfig = {
  async preVisit(page) {
    await injectAxe(page);
  },
  async postVisit(page, context) {
    const storyContext = await getStoryContext(page, context);
    if (storyContext.parameters?.a11y?.disable) return;
    await configureAxe(page, {
      rules: storyContext.parameters?.a11y?.config?.rules,
    });
    await checkA11y(page, "#storybook-root", {
      detailedReport: true,
      detailedReportOptions: { html: true },
      axeOptions: storyContext.parameters?.a11y?.options,
    });
  },
};

export default config;
