# App-level KV isolation

**Status:** App scoping and adversarial regression coverage are implemented.
This document defines the app-isolation contract. End-user authorization is
outside this scope.

## Contract

An app cannot read, overwrite, delete, increment, inspect expiry, change expiry,
conditionally create, or list another app's KV entries. Knowing the other app's
identity or a stored key does not confer access.

The worker and native runtime are trusted to select the app identity. Every
request issued by creator JavaScript uses the KV handle installed for that app.
All end users of an app reach the same app-owned KV namespace; there is no
end-user identity or role check in the KV storage path.

```text
Worker process
|
+-- App A isolate -> env.kv -> native Kv bound to A -> {A}:key
|
+-- App B isolate -> env.kv -> native Kv bound to B -> {B}:key
                                                      |
                                                      v
                                            Configured shared backend
```

This contract uses the existing in-process native binding. A separate KV
service, resource-grant protocol, or Redis logical database per app is not a
prerequisite. Backend selection remains host runtime configuration.

## Enforcement

- The worker supplies the app identity from its resolved app context when it
  builds the runtime. Creator vars, secrets, request fields, and mutable
  JavaScript properties do not select this identity. Deployed runtimes must not
  share a fallback identity when host configuration is missing.
- `KvBinding::build_instance` validates that identity and creates a scoped `Kv`
  handle. The private Rust handle fixes its namespace before creator operations
  execute. Changing `process.env.APP_ID`, adding an `app_id` property, or passing
  an extra namespace option cannot rebind it.
- Every operation uses the handle's namespace. Key validation prevents fully
  scoped keys from being injected into ordinary key arguments.
- Listing applies the namespace as well as the requested literal prefix.
  Redis glob characters are escaped. Returned keys are checked against the app
  scope. Opaque or forged cursors cannot change the app being listed.
- Creator code receives no raw `Backend`, `KvStore`, Redis connection, or storage
  credentials. Trusted Rust hosts retain those construction capabilities;
  application-facing Rust code receives a scoped `Kv`.
- Platform-private storage credentials and administrative access stay outside
  creator-accessible interfaces. The namespace is not a credential boundary for
  arbitrary trusted Rust callers.

The current identity path is in
[worker runtime construction](../../crates/zeroship-worker/src/cache.rs),
[runtime plugin construction](../../crates/zeroship-runtime/src/core/plugin.rs),
and [the KV binding](../../crates/zeroship-kv-v8/src/lib.rs).
The storage contract is in [Kv](../../crates/zeroship-kv/src/store.rs),
[Namespace](../../crates/zeroship-kv/src/namespace.rs), and the
[backends](../../crates/zeroship-kv/src/backend/mod.rs).

## Verification work

Exercise separate V8 isolates sharing the same plugin and storage backend on a
worker thread. Seed colliding key names with distinguishable values, operate as
an app, then inspect the other app through a separately bound Rust handle.
Check the allowed local effect as well as the absence of a foreign effect.

Cover the entire native KV operation surface. Include creator environment
spoofing before module evaluation, JavaScript identity mutation, extra options,
forged native receivers, fully scoped key injection, glob prefixes, malformed
cursors, pagination, and concurrent pending operations from different isolates.
A scan must discover the expected local keys, so an empty result cannot stand
in for proof of isolation.

Run the same V8 isolation contract against temporary redb storage, an owned
Redis container, and an owned Dragonfly cluster. These are ordinary Cargo tests;
container startup failure fails the run. Preserve the existing image-tag policy
and Testcontainers fixture ownership.

The regression lives in
[the V8 isolation suite](../../crates/zeroship-kv-v8/tests/isolation.rs).
Run it with:

```sh
cargo test -p zeroship-kv-v8 --test isolation
```

Also run the existing storage and V8 suites after any production change to the
identity, namespace, dispatch, or backend path. Changes remain limited to KV
and necessary host wiring. Other primitives and DB/ORM work remain independent.

## Meaning of the guarantee

The guarantee applies to operations issued by creator apps through the runtime
and its scoped KV API. It depends on the V8/native runtime and the host's app
identity being trusted, and on creator code having no alternate route to raw
storage credentials or administrator interfaces.

Containing arbitrary native compromise of a shared worker requires a different
process/VM isolation contract. End-user row/key permissions, deliberately shared
KV resources, resource quotas, and a standalone KV service are separate design
questions rather than dependencies of this app-isolation change.
