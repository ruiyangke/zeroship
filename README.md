# zeroship

**A platform for building, deploying, and running applications from local source.**

Creators and coding agents build applications locally, package them as content-addressed `.zship` artifacts, and deploy them to zeroship.

The platform provides a V8-per-thread worker runtime, managed database, authentication, key-value storage, object storage, and request routing.

A small `env.*` native-primitive kernel exposes runtime capabilities. `@zeroship/*` npm packages provide the higher-level developer APIs.

The gateway uses each application's manifest to route requests, serve assets, and dispatch application code to workers.

---

For implementation details, start at [`AGENTS.md`](./AGENTS.md).
