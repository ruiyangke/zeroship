/**
 * DOM environment for the React-adapter tests (`react.test.tsx`).
 *
 * Loaded via `node --import ./tests/setup-dom.ts` BEFORE any test module, so
 * `window`, `document`, `history`, `location`, and the rest of the browser
 * globals exist when React / @testing-library / `@zeroship/auth/react` first
 * evaluate. `react-test-renderer` would suffice for hooks, but the adapter's
 * mount path reads `window.location.search` + calls `history.replaceState`, and
 * `SignInButton` needs a REAL DOM click event to prove the gesture is
 * preserved — both require an actual DOM, which happy-dom provides in Node.
 *
 * The server-entry / client tests don't touch the DOM and are unaffected by the
 * registered globals (they inject their own `ClientEnv` / `env.auth`).
 */

import { GlobalRegistrator } from "@happy-dom/global-registrator";

GlobalRegistrator.register({ url: "https://myapp.zeroship.test/" });

// React 19's `act()` looks for this flag to enable the test-only act() path.
(globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

process.on("exit", () => {
  // Best-effort teardown; `unregister()` is async but `exit` can't await — the
  // process is ending anyway, so restoring globals is moot. Kept for symmetry
  // with happy-dom's documented lifecycle.
  void GlobalRegistrator.unregister();
});
