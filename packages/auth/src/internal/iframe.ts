/**
 * Immersive-iframe orchestration (the drop-in for the popup WINDOW, §3/§4.1).
 *
 * Where the federated popup opens a `window.open` and navigates it, the
 * first-party password login on the same-site console embeds
 * `auth.zeroship.ai/login` inside an in-page `<iframe>`. The whole
 * `authorize → /login → consent → popup-callback` dance runs INSIDE that one
 * frame as a chain of top-level navigations within it; the app-origin callback
 * `postMessage`s `{code,state}` to `window.parent` (the console top), where the
 * SDK relay listener (registered on the top window) receives it.
 *
 *   - `createIframe(env, url)` mounts the iframe with its `src` PRE-SET to the
 *     authorize URL. The navigation CONTRACT (load-bearing, §4.1): an iframe to
 *     a cross-origin document CANNOT be steered by assigning
 *     `iframe.contentWindow.location.href` — that throws `SecurityError`. We set
 *     the ELEMENT `src` (the attribute), which the browser is allowed to
 *     navigate cross-origin. (The analogous bug `popup.location.href` would hit
 *     here is exactly why this driver does NOT mirror `runPopup`'s navigate
 *     step.)
 *   - `runIframe(env, frame, url, relay)` `Promise.race`s the relay against a
 *     60 s timeout (mirroring `runPopup`), and on settle REMOVES the iframe
 *     element (the iframe analogue of `popup.close()`). There is NO `closed`
 *     poll — an iframe has no `.closed`; the modal's close button is the cancel
 *     signal (it rejects the race externally; §8).
 */

import { AuthError } from "../types";
import type { IframeLike, ResolvedEnv } from "./env";
import type { AuthorizationResponse, RelayHandle } from "./relay";

/**
 * Create + mount the cross-origin login iframe with `src` PRE-SET to the
 * authorize `url`. Returns `null` when the environment has no iframe factory
 * (no DOM) — the caller then falls back to the popup.
 */
export function createIframe(env: ResolvedEnv, url: string): IframeLike | null {
  if (!env.createIframe) return null;
  try {
    return env.createIframe(url);
  } catch {
    return null;
  }
}

/**
 * Drive the iframe: await the relay, enforce the 60 s timeout, and tear the
 * iframe element down on settle. Resolves with the authorization response, or
 * rejects with a typed {@link AuthError} (`timeout` / the relay's error
 * mapping). Unlike the popup there is NO `closed` poll: the iframe cannot report
 * a user-close; the modal's own close affordance disposes the relay externally.
 *
 * NOTE: the iframe is created by the caller WITH `src` already set (§4.1) — this
 * driver never assigns `frame.src` again, and NEVER touches
 * `contentWindow.location` (which would throw `SecurityError` cross-origin).
 */
export async function runIframe(
  env: ResolvedEnv,
  frame: IframeLike,
  _url: string,
  relay: RelayHandle,
): Promise<AuthorizationResponse> {
  let timeoutId: number | undefined;
  const { timeoutMs } = env.popupTiming;
  const timeout = new Promise<never>((_, reject) => {
    timeoutId = env.window.setTimeout(() => {
      reject(new AuthError("timeout", `sign-in iframe timed out after ${timeoutMs}ms`));
    }, timeoutMs);
  });

  // The modal's close affordance (if the host wired `frame.cancelled`) rejects
  // the race with `popup_closed` — the iframe analogue of the popup-close hint.
  const cancel: Promise<never> = frame.cancelled
    ? frame.cancelled.then(() => {
        throw new AuthError("popup_closed", "the sign-in modal was closed");
      })
    : new Promise<never>(() => {
        /* never settles — no cancel channel */
      });

  try {
    // The relay (any of the three channels) wins over the cancel/timeout race.
    const response = await Promise.race([relay.promise, cancel, timeout]);
    return response;
  } finally {
    if (timeoutId !== undefined) env.window.clearTimeout(timeoutId);
    relay.dispose();
    try {
      frame.remove();
    } catch {
      // already detached / inaccessible — fine
    }
  }
}
