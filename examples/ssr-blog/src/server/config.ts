import { defineApp } from "@zeroship/server";

// ssr-blog is a PUBLIC demo: `listPosts` serves a fixed list of posts with no
// per-user data. The worker calls it in process to render each document, and
// the hydrated page calls it again from the browser, which holds no session.
// Under the fail-closed default every RPC procedure resolves to `auth: user`,
// so that browser call is refused 401 at the gateway. Opt it into anonymous;
// `publiclyAccessible: true` is the confirmation the manifest validator
// requires alongside `auth: "anonymous"`.
export default defineApp({
  resources: {
    "rpc:listPosts": { auth: "anonymous", publiclyAccessible: true },
  },
});
