import { defineApp } from "@zeroship/server";

// App resource policy. The most important thing it controls here is RPC AUTH.
//
// SEC-5 fail-closed default: when deployed behind the gateway, EVERY RPC
// procedure requires an authenticated end-user by default (a procedure with no
// auth policy resolves to `auth: "user"`). Forgetting to set auth yields a loud
// 401 — never a silent public endpoint.
//
// This starter is a PUBLIC demo (no login / no per-user data), so we opt its two
// procedures into anonymous access. `publiclyAccessible: true` is the deliberate
// confirmation the manifest validator requires alongside `auth: "anon"`, making
// "this endpoint is intentionally public" explicit and reviewable.
//
// When your app has real user data, DROP these entries (or set `auth: "user"`)
// and read identity inside the handler with `env.auth.getUser()` /
// `requireUser()`. See docs/reference/rpc.md — "Procedure auth".
export default defineApp({
  resources: {
    "rpc:getMessages": { auth: "anon", publiclyAccessible: true },
    "rpc:addMessage": { auth: "anon", publiclyAccessible: true },
  },
});
