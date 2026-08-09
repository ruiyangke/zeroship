import { defineApp } from "@zeroship/server";

// Anonymously reachable ON PURPOSE.
//
// The platform default is fail-closed (`auth: "user"`), and a gated procedure is
// answered by the GATEWAY's auth gate before dispatch - so it never reaches the
// worker and the deployed response would describe the gateway's refusal, not
// the app's environment. That failure mode reads as a clean "no leak", which is
// exactly the false green `tests/e2e_dev_vs_deployed_env.sh` exists to avoid.
// The harness re-asserts this `anon` declaration against the BUILT manifest.
export default defineApp({
  resources: {
    "rpc:envp.report": { auth: "anon", publiclyAccessible: true },
  },
});
