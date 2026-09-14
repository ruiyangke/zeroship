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
    // Raise this tab to the foreground before any play() runs. Headless
    // Chromium backgrounds freshly-opened tabs; a backgrounded tab can
    // throttle timers and the async focus moves that focus-trap /
    // focus-return / roving-tabindex assertions depend on, so the
    // settle-before-assert window becomes nondeterministic. `bringToFront`
    // is cheap and keeps the tab active for the whole visit. (It is NOT
    // sufficient on its own — the stories that assert post-interaction
    // focus poll with `waitFor` so the assertion waits for the async focus
    // move to commit rather than racing it.)
    await page.bringToFront();
    // Emulate `prefers-reduced-motion: reduce` for the whole run. Every
    // component gates its enter/leave transitions behind a reduced-motion
    // block, so without this a synchronous `toBeVisible()` in a play() races
    // the entrance transition (popups go opacity 0 → 1 over --zs-motion-base)
    // and fails deterministically. Reduced-motion is a real supported path
    // the components are built for, so testing under it is faithful — and it
    // makes overlay open-state assertions deterministic.
    await page.emulateMedia({ reducedMotion: "reduce" });
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
