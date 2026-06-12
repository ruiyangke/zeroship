import { defineApp } from "@zeroship/server";

// csr-todo is a PUBLIC demo: `listTodos` / `searchTodos` serve a static,
// in-memory todo list with no per-user data, and the SPA fetches them from the
// browser with no session. Under the SEC-5 fail-closed default every RPC
// procedure resolves to `auth: user`, so an anonymous browser call 401s at the
// gateway (see ISS-69). Opt these two read-only procedures into anon so the
// public SPA reaches them through the gateway. `publiclyAccessible: true` is the
// deliberate confirmation the manifest validator requires alongside
// `auth: "anon"`. See docs/reference/rpc.md — "Procedure auth".
export default defineApp({
  resources: {
    "rpc:listTodos": { auth: "anon", publiclyAccessible: true },
    "rpc:searchTodos": { auth: "anon", publiclyAccessible: true },
  },
});
