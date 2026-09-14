import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// auth-probe is a SERVER-ONLY app (like examples/auth-notes): no index.html,
// no client bundle - only `src/index.ts` exporting RPC procedures.
//
// The two dev users below are the DEV half of a dev-vs-deployed identity pair.
// `tests/e2e_dev_vs_deployed_auth.sh` offline-mints a gateway session cookie
// carrying the SAME id / email / name / avatar / email_verified / scopes for the
// deployed half, so `env.auth.getUser()` can be diffed BYTE-FOR-BYTE instead of
// through a normaliser that would hide exactly the field-level divergence the
// comparison exists to find. Change a value here and you MUST change the
// matching claim in that script (it re-asserts the pair on every run).
//
// The dev sign-in password is NOT declared here - it is derived from each id by
// `devPasswordFor` (packages/vite-plugin/src/dev-auth.ts): "dev-" + the first 8
// characters after "pws_". So alpha signs in with "dev-probealp" and beta with
// "dev-probebet". The e2e harnesses derive it the same way from the ids below.
//
// alpha carries a non-null avatar, beta carries a null one. That is the
// one-variable control pair for the `avatar` field: the gateway's WorkerUser
// declares `#[serde(skip_serializing_if = "Option::is_none")]` on `avatar`
// while the dev provider's `normalizeUser` always emits `avatar: u.avatar ?? null`.
const SCOPES = ["openid", "profile", "email"];

export default defineConfig({
  plugins: [
    zeroship({
      devServerPort: Number(
        (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env
          ?.AUTH_PROBE_API_PORT ?? 3091,
      ),
      devAuth: {
        users: [
          {
            id: "pws_probealpha0000000000",
            email: "alpha@probe.zeroship.test",
            name: "Probe Alpha",
            avatar: "https://probe.zeroship.test/a.png",
            scopes: SCOPES,
          },
          {
            id: "pws_probebeta00000000000",
            email: "beta@probe.zeroship.test",
            name: "Probe Beta",
            avatar: null,
            scopes: SCOPES,
          },
        ],
        defaultUserId: "pws_probealpha0000000000",
      },
    }),
  ],
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
