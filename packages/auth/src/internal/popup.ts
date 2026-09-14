/**
 * Popup orchestration (gateway §4.4).
 *
 *   - `openPopup()` calls `window.open('', 'zs:auth', features)` SYNCHRONOUSLY
 *     inside the user gesture (before any async URL build) so a popup blocker
 *     does not fire. A `null` return ⇒ `popup_blocked`.
 *   - `runPopup(popup, url, relay)` navigates the popup to `url`, awaits the
 *     relay response, and polls `popup.closed` every 1000 ms (a HINT — only
 *     emits `popup_closed` when NO relay arrived, because COOP can sever the
 *     opener and read `closed === true` prematurely). A 60 s timeout rejects
 *     with `timeout`.
 */

import { AuthError } from "../types";
import type { ResolvedEnv, WindowProxyLike } from "./env";
import type { AuthorizationResponse, RelayHandle } from "./relay";

const POPUP_FEATURES = "width=500,height=650,menubar=no,toolbar=no,location=no,status=no";

/**
 * Open the popup synchronously in the click handler (empty URL — navigated
 * later by `runPopup`). Returns `null` when blocked.
 */
export function openPopup(env: ResolvedEnv): WindowProxyLike | null {
  try {
    return env.window.open("", "zs:auth", POPUP_FEATURES);
  } catch {
    return null;
  }
}

/**
 * Drive the popup: navigate to `url`, await the relay, poll `closed`, enforce
 * the 60 s timeout. Resolves with the authorization response, or rejects with
 * a typed {@link AuthError} (`popup_closed` / `timeout` / `config_error` / the
 * relay's error mapping).
 *
 * `closed === true` is treated as a HINT: it only triggers `popup_closed` when
 * no relay response has arrived, so a COOP-induced early `closed` cannot cancel
 * a flow whose code already came back over BroadcastChannel/localStorage.
 */
export async function runPopup(
  env: ResolvedEnv,
  popup: WindowProxyLike,
  url: string,
  relay: RelayHandle,
): Promise<AuthorizationResponse> {
  // Navigate the already-open popup to the authorize URL.
  try {
    popup.location.href = url;
    popup.focus?.();
  } catch (cause) {
    relay.dispose();
    throw new AuthError("config_error", "could not navigate popup", { cause });
  }

  let pollId: number | undefined;
  let timeoutId: number | undefined;
  const clearTimers = () => {
    if (pollId !== undefined) env.window.clearInterval(pollId);
    if (timeoutId !== undefined) env.window.clearTimeout(timeoutId);
  };

  const { pollMs, timeoutMs } = env.popupTiming;
  const closedOrTimeout = new Promise<never>((_, reject) => {
    pollId = env.window.setInterval(() => {
      let isClosed = false;
      try {
        isClosed = popup.closed;
      } catch {
        // Cross-origin access to `closed` can throw under COOP — treat as a
        // hint we cannot read, NOT as closed.
        isClosed = false;
      }
      if (isClosed) {
        reject(new AuthError("popup_closed", "the sign-in popup was closed"));
      }
    }, pollMs);
    timeoutId = env.window.setTimeout(() => {
      reject(new AuthError("timeout", `sign-in popup timed out after ${timeoutMs}ms`));
    }, timeoutMs);
  });

  try {
    // The relay (any of the three channels) wins over the closed/timeout race.
    const response = await Promise.race([relay.promise, closedOrTimeout]);
    return response;
  } finally {
    clearTimers();
    relay.dispose();
    try {
      popup.close();
    } catch {
      // already closed / inaccessible — fine
    }
  }
}
