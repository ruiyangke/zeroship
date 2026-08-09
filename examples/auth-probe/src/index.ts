"use server";

// auth-probe - the AUTH leg of the dev-vs-deployed seam comparison
// (docs/pilot/e2e-scenarios.md scenario 6 / scenario 11).
//
// Sibling of `examples/storage-probe` and `examples/workflow-probe`: a
// deliberately boring app whose only job is to let `tests/e2e_dev_vs_deployed_auth.sh`
// run ONE identical sequence against `pnpm dev` and against the same `.zship`
// deployed behind the gateway, and diff the RESULTS.
//
// WHY A NEW EXAMPLE RATHER THAN `examples/auth-notes`. auth-notes stores notes
// in `env.kv`, so half its procedures need a Redis on the deployed side, and the
// deployed harness would be comparing auth AND a kv backend at once. This app
// has NO backend primitive at all - only `env.auth` - so any divergence it
// reports is an auth divergence.
//
// THE FIVE AUTH POSTURES, one procedure each. The posture lives in
// `src/server/config.ts`; the difference between them is the whole point:
//
//   probe.public       auth:"anon"  - reachable anonymously on both sides.
//   probe.defaulted    (undeclared) - SEC-5 fail-closed default resolves it to
//                                     `auth:"user"`. THE #163 PROBE: deployed
//                                     the gateway refuses an anonymous caller,
//                                     dev has no gateway and therefore no gate.
//   probe.userDeclared auth:"user"  - the same gate, declared explicitly.
//   probe.requireAnon  auth:"anon"  - RAW `auth.requireUser()` reachable
//                                     anonymously, so the KERNEL's own failure
//                                     shape is observable on BOTH sides (a
//                                     gated procedure never reaches the worker
//                                     deployed, so it cannot show this).
//   probe.appGate      auth:"anon"  - an app-level `getUser()` gate throwing a
//                                     status-bearing 401. Handler code, so it
//                                     SHOULD be identical on both sides; it is
//                                     the control that separates "the platform
//                                     gate diverges" from "everything diverges".
//
// Every procedure returns the RAW `env.auth.getUser()` value, unmapped. That is
// deliberate: `getUser()` is a bare `JSON.parse` of the `ZeroShip-User` payload
// (crates/runtime/src/auth.rs::get_user_callback), so the raw object IS the
// kernel contract surface - key names, key order, and which keys are present at
// all. Passing it through `@zeroship/auth`'s camelCase `User` type here would
// launder exactly the field-level divergence the comparison exists to find.

import { auth } from "@zeroship/auth";
import { query } from "@zeroship/rpc/server";

/** The raw `ZeroShip-User` projection, exactly as the kernel hands it over. */
type RawUser = Record<string, unknown> | null;

function raw(): RawUser {
  return auth.getUser() as unknown as RawUser;
}

/** `probe.public` - anonymous-reachable identity echo. */
export const publicProbe = query(
  async (): Promise<{ ok: true; user: RawUser }> => ({ ok: true, user: raw() }),
  { id: "probe.public" },
);

/**
 * `probe.defaulted` - DELIBERATELY ABSENT from `src/server/config.ts`.
 *
 * Do not "fix" this by adding it there. The SEC-5 default resolving it to
 * `auth:"user"` is the behaviour under test: it is the posture every procedure
 * of `examples/kv-dashboard` and `examples/auth-uploads-kv` shipped with, and
 * the one whose dev-vs-deployed answer differs (#163).
 */
export const defaultedProbe = query(
  async (): Promise<{ ok: true; user: RawUser }> => ({ ok: true, user: raw() }),
  { id: "probe.defaulted" },
);

/** `probe.userDeclared` - the same gate as `probe.defaulted`, declared. */
export const userDeclaredProbe = query(
  async (): Promise<{ ok: true; user: RawUser }> => ({ ok: true, user: raw() }),
  { id: "probe.userDeclared" },
);

/**
 * `probe.requireAnon` - RAW `auth.requireUser()` with NO app-level catch, on an
 * anonymously-reachable procedure. An anonymous call therefore surfaces the
 * KERNEL's throw verbatim on both sides (status, code, message), which a
 * `auth:"user"` procedure cannot show deployed because the gateway answers
 * first.
 */
export const requireAnonProbe = query(
  async (): Promise<{ user: unknown }> => ({ user: auth.requireUser() }),
  { id: "probe.requireAnon" },
);

/** `probe.requireGated` - the same raw `requireUser()` behind `auth:"user"`. */
export const requireGatedProbe = query(
  async (): Promise<{ user: unknown }> => ({ user: auth.requireUser() }),
  { id: "probe.requireGated" },
);

/**
 * `probe.appGate` - the pattern `docs/reference/auth.md` tells creators to use
 * when they want their own 401: gate on `getUser()` and throw a status-bearing
 * error. Pure handler code with no platform gate involved, so it is the CONTROL
 * for every other row: if this one diverges too, the finding is not about auth
 * postures.
 */
export const appGateProbe = query(
  async (): Promise<{ user: RawUser }> => {
    const user = raw();
    if (!user) {
      throw Object.assign(new Error("Authentication required"), {
        status: 401,
        code: "UNAUTHENTICATED",
      });
    }
    return { user };
  },
  { id: "probe.appGate" },
);

/**
 * `probe.userShape` - the field surface of the raw projection, stated as data
 * rather than inferred from a serialised object: which keys exist, in what
 * order the kernel emitted them, and whether `avatar` is PRESENT-AND-NULL or
 * ABSENT. `JSON.stringify` alone cannot distinguish "avatar omitted" from
 * "avatar null" once you start normalising values, and that distinction is a
 * measured divergence between the two tiers.
 */
export const userShapeProbe = query(
  async (): Promise<{
    anonymous: boolean;
    keys: string[];
    hasAvatar: boolean;
    avatarIsNull: boolean;
    hasEmailVerifiedSnake: boolean;
    hasEmailVerifiedCamel: boolean;
  }> => {
    const user = raw();
    if (!user) {
      return {
        anonymous: true,
        keys: [],
        hasAvatar: false,
        avatarIsNull: false,
        hasEmailVerifiedSnake: false,
        hasEmailVerifiedCamel: false,
      };
    }
    return {
      anonymous: false,
      keys: Object.keys(user),
      hasAvatar: Object.prototype.hasOwnProperty.call(user, "avatar"),
      avatarIsNull: user.avatar === null,
      hasEmailVerifiedSnake: Object.prototype.hasOwnProperty.call(user, "email_verified"),
      hasEmailVerifiedCamel: Object.prototype.hasOwnProperty.call(user, "emailVerified"),
    };
  },
  { id: "probe.userShape" },
);
