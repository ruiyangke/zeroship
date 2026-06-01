"use server";
//
// Console fetch handler. Under the BFF cutover the console is a regular
// gateway-fronted zeroship app: end-user (creator) auth runs entirely
// through the platform — the gateway's `/__zeroship/auth/*` endpoints + the
// `@zeroship/auth` browser SDK — NOT a bespoke OAuth RP in the app.
//
// So `builderFetch` no longer owns `/auth/{login,callback,logout}`; the
// retired RP modules (oauth.ts / session.ts / oauth-store.ts) are gone.
// Identity arrives on every request as the gateway-forwarded
// `ZeroShip-User` envelope, exposed to app code as `currentUser()`. The
// console is a pure creator app — it holds no control credential and
// makes no control calls. This handler only forwards the live preview
// proxy; the RPC procedures are dispatched by the runtime from the
// `server.ts` re-exports.

import { previewFetch } from "./preview-proxy.js";

export async function builderFetch(
  request: Request,
  env: unknown,
  ctx: unknown,
): Promise<Response> {
  void env;
  void ctx;
  return previewFetch(request);
}
