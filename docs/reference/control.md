# `@zeroship/control`

`@zeroship/control` is the TypeScript client for the control-plane HTTP API.
It is for platform-owned code: the Builder app, CLIs, internal agents, and
tests that need to create apps, deploy `.zship` artifacts, manage environment
configuration, or drive dashboard auth.

Creator apps should not import this package. Creator-facing app code talks to
runtime SDKs such as `@zeroship/db`, `@zeroship/kv`, `@zeroship/auth`, and
`@zeroship/rpc`; the control plane is an operator/admin surface.

The Rust API lives in `crates/control/src/{api,auth_handlers,env_handlers}.rs`.
The TypeScript client lives in `sdks/control/src/index.ts`.

## Client setup

```ts
import { createControlClient } from "@zeroship/control";

const control = createControlClient({
  baseUrl: "http://localhost:9090",
  auth: () => process.env.CONTROL_KEY,
});

const apps = await control.apps.list();
```

`auth` is a master key or bearer token provider. Values without an auth scheme
are sent as `Authorization: Bearer <value>`.

For server-side proxies that authenticate through the dashboard session cookie,
forward the inbound cookie and mirror upstream `Set-Cookie` headers:

```ts
const control = createControlClient({
  baseUrl: CONTROL_URL(),
  cookie: () => getRequest()?.headers.get("cookie") ?? null,
  onSetCookie: (cookie) => {
    getResponseHeaders()?.append("Set-Cookie", cookie);
  },
});
```

## Namespaces

The public API is grouped by control-plane domain:

```ts
await control.apps.create({ name: "demo", plan_id: "free" });
await control.apps.deploy(appId, zshipBytes);
await control.apps.setPlan(appId, { plan_id: "pro" });
await control.apps.logs(appId);

await control.env.setVar(appId, { key: "PUBLIC_URL", value: "https://..." });
await control.env.setSecret(appId, { key: "OPENAI_API_KEY", value: "sk-..." });
await control.env.setExpose(appId, { keys: ["OPENAI_API_KEY"] });
await control.env.listAudit(appId, { limit: 100 });

await control.auth.login({ email, password });
await control.auth.userinfo();
await control.auth.logout();
```

`control.request<T>(path, options)` is the escape hatch for endpoints that do
not yet deserve a typed wrapper. Prefer adding a typed method once a caller
appears in product code.

## Deploys

`control.apps.deploy(appId, artifact)` sends `application/x-zship` by default.
The control plane no longer accepts raw JavaScript deploy bodies; callers should
upload the `.zship` artifact emitted by the build pipeline.

```ts
const artifact = await fs.promises.readFile("dist/app.zship");
const result = await control.apps.deploy(appId, artifact);
console.log(result.deploy_hash);
```

## Errors

Non-2xx responses throw `ControlError`:

```ts
import { ControlError } from "@zeroship/control";

try {
  await control.auth.userinfo();
} catch (error) {
  if (error instanceof ControlError && error.status === 401) {
    return null;
  }
  throw error;
}
```

`ControlError` carries `status`, `statusText`, parsed `body`, optional `code`,
and the original `response`. Branch on status/code, not message text.

## Design rules

- Keep this package framework-neutral. Do not import React, Vite, or Builder
  internals.
- Keep auth explicit. Browser/server cookie forwarding belongs in the caller's
  setup, not hidden global state.
- Use typed namespace methods for stable control-plane endpoints.
- Use `control.request()` only as a temporary escape hatch.
- Add tests when adding a method, especially for headers, body encoding,
  `204` handling, and error parsing.
