/**
 * Popup-callback relay listener (gateway §4.4).
 *
 * The `/__zeroship/auth/popup-callback` page posts the SAME envelope over three
 * SAME-ORIGIN channels (so a COOP-severed `window.opener` still delivers):
 *
 *   { type: 'zs:authorization_response',
 *     response: { code, state } | { error, error_description, state } }
 *
 *   1. `window.opener.postMessage(msg, location.origin)`  (primary)
 *   2. `new BroadcastChannel('zs:auth').postMessage(msg)`  (opener severed)
 *   3. one-shot `localStorage['@@zsauth@@::relay::<state>'] = JSON(msg)`  (storage event)
 *
 * `waitForResponse` validates origin + envelope type on the postMessage leg,
 * subscribes to all three, and resolves with the FIRST matching response whose
 * `state` equals the flow's expected `state`.
 * A message from the wrong origin surfaces a distinct `config_error` rather
 * than silently waiting for the timeout (gateway §4.4).
 *
 * ## State filtering (MAJOR fix)
 *
 * All three channels are ORIGIN-shared: a `BroadcastChannel('zs:auth')` and an
 * origin-wide `storage` event are delivered to EVERY tab/flow on the origin,
 * and a stale message can linger. Without filtering, two concurrent sign-in
 * flows (or a stale relay payload from a prior flow) could cross-deliver a code
 * to the WRONG flow. The fix threads the flow's expected `state` into the
 * listener and IGNORES (keeps waiting — does NOT reject) any envelope whose
 * `response.state` does not match. Only the flow's OWN envelope settles it.
 */

import { AuthError } from "../types";
import type {
  BroadcastChannelLike,
  MessageEventLike,
  ResolvedEnv,
  StorageEventLike,
} from "./env";

const MESSAGE_TYPE = "zs:authorization_response";
const RELAY_STORAGE_PREFIX = "@@zsauth@@::relay::";

/** The decoded authorization response payload. */
export interface AuthorizationResponse {
  code?: string;
  state?: string;
  error?: string;
  error_description?: string;
}

interface Envelope {
  type?: string;
  response?: AuthorizationResponse;
}

/** Parse + shallow-validate a channel payload into an envelope, or null. */
function asEnvelope(data: unknown): Envelope | null {
  if (!data || typeof data !== "object") return null;
  const env = data as Envelope;
  if (env.type !== MESSAGE_TYPE) return null;
  if (!env.response || typeof env.response !== "object") return null;
  return env;
}

export interface RelayHandle {
  /** Resolves with the first matching authorization response. */
  readonly promise: Promise<AuthorizationResponse>;
  /** Tear down every channel subscription (call after resolve/reject/timeout). */
  dispose(): void;
}

/**
 * Listen on all three relay channels for a `zs:authorization_response`.
 *
 * `expectedOrigin` MUST equal the app origin — a postMessage from any other
 * origin is rejected with `config_error`. The BroadcastChannel and localStorage
 * legs are inherently same-origin so they carry no cross-origin exposure.
 *
 * `expectedState` is the flow's PKCE `state`. Every channel is origin-shared,
 * so a well-formed `zs:authorization_response` from a CONCURRENT flow (or a
 * stale prior message) can arrive here; any envelope whose `response.state`
 * does not equal `expectedState` is IGNORED (the listener keeps waiting — it is
 * NOT a rejection), so only THIS flow's response settles it (MAJOR fix).
 */
export function listenForRelay(
  env: ResolvedEnv,
  expectedOrigin: string,
  expectedState: string,
): RelayHandle {
  let settle!: (r: AuthorizationResponse) => void;
  let fail!: (e: unknown) => void;
  let done = false;

  const promise = new Promise<AuthorizationResponse>((resolve, reject) => {
    settle = resolve;
    fail = reject;
  });

  const cleanups: Array<() => void> = [];
  const dispose = () => {
    if (done) return;
    done = true;
    for (const c of cleanups) {
      try {
        c();
      } catch {
        // ignore teardown errors
      }
    }
  };
  // Settle ONLY for this flow's own response. A mismatched `state` is from a
  // concurrent flow (or a stale message) on the origin-shared channels — keep
  // waiting (return), do NOT reject and do NOT tear down the other flow.
  const settleIfMine = (r: AuthorizationResponse) => {
    if (done) return;
    if (r.state !== expectedState) return; // not ours — ignore, keep waiting
    settle(r);
    dispose();
  };
  const rejectOnce = (e: unknown) => {
    if (done) return;
    fail(e);
    dispose();
  };

  // (1) postMessage to the opener — validate origin + envelope.
  //
  // `asEnvelope` returns null (→ silently ignored) for ANY message that is not
  // a well-formed `zs:authorization_response` — so a third-party library's
  // unrelated postMessage never reaches the origin check and cannot tear the
  // relay down. Only a message that IS a `zs:authorization_response` envelope
  // yet arrives from the wrong origin is a genuine misconfiguration, and THAT
  // is rejected with `config_error` (rather than silently waiting for the 60s
  // timeout). Corollary: do not mix a third-party sender that emits envelopes
  // typed `zs:authorization_response` from a foreign origin — the SDK treats
  // such a message as a security misconfiguration.
  const onMessage = (ev: MessageEventLike) => {
    const envl = asEnvelope(ev.data);
    if (!envl) return; // not ours — ignore (other libraries postMessage too)
    if (ev.origin !== expectedOrigin) {
      rejectOnce(
        new AuthError(
          "config_error",
          `authorization_response from unexpected origin ${ev.origin} (expected ${expectedOrigin})`,
        ),
      );
      return;
    }
    settleIfMine(envl.response!);
  };
  env.window.addEventListener("message", onMessage);
  cleanups.push(() => env.window.removeEventListener("message", onMessage));

  // (2) BroadcastChannel — same-origin by construction.
  if (env.broadcastChannel) {
    let bc: BroadcastChannelLike | undefined;
    try {
      bc = env.broadcastChannel("zs:auth");
      bc.onmessage = (m) => {
        const envl = asEnvelope(m.data);
        if (envl) settleIfMine(envl.response!);
      };
      cleanups.push(() => bc?.close());
    } catch {
      // BroadcastChannel unavailable — rely on the other two channels.
    }
  }

  // (3) one-shot localStorage relay — same-origin `storage` event.
  if (env.onStorage) {
    const off = env.onStorage((ev: StorageEventLike) => {
      if (!ev.key || !ev.key.startsWith(RELAY_STORAGE_PREFIX) || !ev.newValue) return;
      try {
        const envl = asEnvelope(JSON.parse(ev.newValue));
        if (envl) settleIfMine(envl.response!);
      } catch {
        // corrupt relay payload — ignore
      }
    });
    cleanups.push(off);
  }

  return { promise, dispose };
}
