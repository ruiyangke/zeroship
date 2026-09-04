"use server";

// error-probe - the ERROR-ENVELOPE leg of the dev-vs-deployed seam comparison.
//
// Sibling of `examples/auth-probe` / `examples/storage-probe`: a deliberately
// boring app whose only job is to let `tests/e2e_dev_vs_deployed_errors.sh` run
// ONE identical sequence against `pnpm dev` and against the same `.zship`
// deployed behind the gateway, and diff the RESULTS.
//
// WHY A NEW EXAMPLE RATHER THAN A ROW IN `examples/auth-probe`. auth-probe's
// harness scrubs `"stack":"..."` down to `"stack":"<STACK>"` before diffing, on
// the (correct) grounds that dev frames are absolute source paths and deployed
// frames are bundle offsets, so the CONTENT can never match. That scrub is
// exactly what makes it blind here: when BOTH tiers emit a stack the rows are
// byte-identical after scrubbing and the diff goes green. A dev-vs-deployed
// comparison alone therefore CANNOT answer "does production leak a stack" - it
// can only answer "do the two tiers disagree". This app exists so its harness
// can assert the ABSOLUTE property (deployed must not ship a stack) alongside
// the relative one.
//
// EVERY PROCEDURE IS `auth: "anon"` (src/server/config.ts). That is load-
// bearing, not laziness: a gated procedure is answered by the GATEWAY's auth
// gate before dispatch, so its error body never touches the worker's
// `build_error_body` at all. Anonymous reachability is the only way a
// worker-originated error envelope is observable on the deployed side.
//
// THE THROW SITE IS SHARED. All four throwing procedures call
// `zsLeakFrameMarker()`, so the message text and the stack shape are held
// constant and the ONLY variable between rows is which properties are attached
// to the thrown error:
//
//   err.plain          (no status, no code)      -> statusFromError defaults 500
//   err.status4xx      status: 403               -> the ONE-VARIABLE partner of
//                                                   err.plain: same throw site,
//                                                   same message, `status` added
//   err.status4xxCode  status: 403, code: "..."  -> the ONE-VARIABLE partner of
//                                                   err.status4xx: `code` added
//   err.publicCode5xx  code: "version_mismatch"  -> the ONE-VARIABLE partner of
//                                                   err.plain: `code` added, and
//                                                   that code is on
//                                                   `is_public_error_code`, so it
//                                                   takes the 5xx sanitizer's
//                                                   EXEMPTION arm
//   err.ok             throws nothing            -> the pass-through control
//
// `err.ok` is what stops a "no leak" verdict from being satisfied by the
// response being empty, blocked, or 404. It returns the same marker string in
// its body, so a green leak-assertion is only meaningful when the control row
// PROVES the transport carries that marker end to end.

// SECOND LEG, added 2026-08-11: DISPATCHER-ORIGINATED errors.
//
// Everything above this line is about errors USER CODE throws. The rows added
// for the dispatcher leg are about the errors produced BEFORE user code runs -
// input parse, unknown procedure id, wrong HTTP method, input-schema rejection.
// Those travel a DIFFERENT rail, and the rail forks by WHICH ERROR it is, not
// by which tier you are on. Measured 2026-08-11, not inferred:
//
//   the SAME Rust runtime binary serves both tiers. `pnpm dev` runs
//   `target/release/zeroship`; the deploy runs `zeroship-worker`. So the parse
//   and dispatch code is shared, and an unparseable body answers with the RUST
//   parser's text on BOTH ("invalid JSON body", from
//   crates/zeroship-runtime/src/core/runtime.rs::parse_rpc_body) rather than the JS
//   text, which would have appended the underlying parse error.
//
//   the DEPLOYED tier has one extra component in front: the gateway. Anything
//   the gateway can answer without asking the worker - an unknown procedure id,
//   a method the procedure kind forbids, a path with no id - it DOES answer,
//   from crates/zeroship-gateway/src/router/dispatch.rs. Those three were the only
//   divergences the leg found, and all three were the gateway speaking a
//   different error envelope from the worker. Fixed in the same change.
//
//   what does NOT reach the unary path on either tier, despite its own header
//   having claimed otherwise until 2026-08-11: `__zsDispatch` from
//   sdks/bootstrap/src/dispatcher.ts. The synthetic entry exports `default.rpc`
//   as a plain dict, and the kernel wraps that dict in `USER_RPC`
//   (crates/zeroship-runtime/src/core/init.rs), which calls a SECOND copy of the
//   dispatch body written inline in that same file as `__zsDispatchRpc`.
//   dispatcher.ts serves the SLOW path only (streams, subscriptions).
//
// `err.needsInput` below is the only procedure in this app that declares an
// input schema, which is what makes the schema-rejection arm reachable at all.

import { z } from "zod";
import { query } from "@zeroship/rpc/server";

/**
 * The marker. It appears in every thrown error's `message`, and in `err.ok`'s
 * success body. Distinctive enough to grep for in a raw response and in the
 * server logs, and it is the same string on both tiers so a diff cannot be
 * satisfied by it differing.
 */
export const LEAK_MARKER = "ZSLEAK-b7f1-marker";

/**
 * The single throw site for every throwing procedure. Named so that the
 * function name is a searchable frame in whatever `Error.stack` the runtime
 * captures - which is the point: if this identifier reaches a client, the stack
 * reached the client.
 */
function zsLeakFrameMarker(extra: Record<string, unknown>): never {
  throw Object.assign(new Error(`boom ${LEAK_MARKER}`), extra);
}

/** `err.plain` - a bare `Error`. No `status`, no `code`. */
export const plainThrow = query(async (): Promise<never> => zsLeakFrameMarker({}), {
  id: "err.plain",
});

/**
 * `err.status4xx` - identical to `err.plain` but for `status: 403`. The
 * one-variable control that separates "the runtime never emits a stack" from
 * "the runtime's sanitizer is gated on the status class".
 */
export const status4xxThrow = query(
  async (): Promise<never> => zsLeakFrameMarker({ status: 403 }),
  { id: "err.status4xx" },
);

/** `err.status4xxCode` - `err.status4xx` plus a `code`. Isolates `code`'s effect at 4xx. */
export const status4xxCodeThrow = query(
  async (): Promise<never> => zsLeakFrameMarker({ status: 403, code: "FORBIDDEN_PROBE" }),
  { id: "err.status4xxCode" },
);

/**
 * `err.publicCode5xx` - `err.plain` plus `code: "version_mismatch"`, which is on
 * `is_public_error_code` in `crates/zeroship-runtime/src/core/dispatch.rs`. No `status`,
 * so it lands at 500 and takes the sanitizer's EXEMPTION arm. The exemption was
 * written to keep the developer-facing `code` on the wire; this row measures
 * what ELSE rides along with it.
 */
export const publicCode5xxThrow = query(
  async (): Promise<never> => zsLeakFrameMarker({ code: "version_mismatch" }),
  { id: "err.publicCode5xx" },
);

/**
 * `err.ok` - throws nothing. The pass-through control: it proves the marker
 * string survives the whole transport (dev server or gateway -> worker -> back)
 * unmodified, so "the marker is absent from the error body" means the body
 * omits it rather than the request having failed or the marker being filtered
 * somewhere generic.
 */
export const okProbe = query(
  async (): Promise<{ ok: true; marker: string }> => ({ ok: true, marker: LEAK_MARKER }),
  { id: "err.ok" },
);

/**
 * `err.needsInput` - the only procedure in this app with an input schema, and
 * the only one whose HANDLER never throws. Every failure it can produce is
 * dispatcher-originated, which is exactly what makes it the vehicle for the
 * dispatcher leg.
 *
 * It is driven three ways by `tests/e2e_dev_vs_deployed_errors.sh`, and the
 * three differ in ONE variable each:
 *
 *   {"json":{"must":"..."}}   the control  -> 200, echoes the marker
 *   {"json":{"WRONGFIELD":1}} field name   -> the schema must REJECT
 *   (empty body)              body present -> input is `undefined`, must REJECT
 *
 * The control is not decoration. "the deployed body carries no stack" and "the
 * deployed body is a 400" are both satisfied by a request that never reached
 * the dispatcher at all - a gateway 404, an unrouted app, a dead worker. The
 * control is the only row that proves this procedure is reachable and running
 * on the tier being measured, so the two rejection rows are rejections rather
 * than absences.
 *
 * `.strict()` is deliberate and load-bearing. Plain `z.object` STRIPS unknown
 * keys rather than rejecting them, so `{"WRONGFIELD":1}` would be rejected only
 * as a side effect of `must` being missing. With `.strict()` the unknown key is
 * itself an error, which is the case the row is named for. `must` stays
 * required so the empty-body row still has something to fail on.
 */
export const needsInput = query(
  async (input: { must: string }): Promise<{ ok: true; echoed: string; marker: string }> => ({
    ok: true,
    echoed: input.must,
    marker: LEAK_MARKER,
  }),
  {
    id: "err.needsInput",
    input: z.object({ must: z.string().min(1) }).strict(),
  },
);
