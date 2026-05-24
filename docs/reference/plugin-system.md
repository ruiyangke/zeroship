# Plugin System

## Overview

The runtime is a kernel. Plugins are drivers. The runtime provides V8, Web APIs (fetch, crypto, console, timers, streams, WebSocket), and a plugin registration API. Platform features (database, KV, storage) are plugins that register native functions as `env.*` namespaces.

Creator code reaches them via `env.<namespace>.*`, where `env` is the 2nd arg to `fetch(req, env, ctx)` and the `env` named export of the `zeroship` module (and the return value of `__zs_env()`). All three are the same frozen object.

Source of truth: `crates/runtime/src/core/plugin.rs` (the trait + registrar + `build_env_object`).

## Design

### NativePlugin trait

```rust
// crates/runtime/src/core/plugin.rs
pub trait NativePlugin: Send + Sync + 'static {
    /// Namespace under `env.*`. Lowercase JS identifier. e.g. "db", "kv", "storage".
    fn namespace(&self) -> &str;

    /// Human-readable name for logging. Defaults to the namespace.
    fn name(&self) -> &str { self.namespace() }

    /// Called once per Runtime, on the thread that owns it. Register V8
    /// callbacks here. May fire repeatedly on the same thread as multiple
    /// Runtimes are constructed (one per app in multi-tenant workers) — so
    /// any thread-local setup done here MUST be idempotent.
    fn register(&self, r: &mut NativeRegistrar);

    /// Optional: build the `env.{namespace}` object yourself instead of
    /// letting the runtime allocate a plain `v8::Object`. Return `Some(obj)`
    /// to ship a `#[v8_class]`-backed instance (internal fields, methods,
    /// getters, finalizers). The runtime STILL runs `register()` and layers
    /// those callbacks on top of the instance. Default `None`.
    fn build_instance<'s>(
        &self,
        _scope: &mut v8::PinScope<'s, '_>,
        _app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        None
    }
}
```

**There is no `init()` or `shutdown()` hook.** Plugins are `Send + Sync + 'static` so they live inside `Arc<dyn NativePlugin>` across worker threads. Per-thread resources (connection pools) are initialized **lazily on first callback**, not in a lifecycle hook — see the `plugin-db` pattern below.

Two registration mechanisms:

| Mechanism | Used by | Shape |
| --- | --- | --- |
| **Flat callbacks** via `register()` + `NativeRegistrar::add` | `plugin-kv`, `plugin-storage` | `env.kv.get`, `env.storage.put`, … attached as plain V8 functions |
| **v8_class instance** via `build_instance()` | `plugin-db` | `env.db` is a `Db` `#[v8_class]` whose methods (`find`, `insert`, …) are `#[v8_method]`s; no per-method `add` call |

A plugin can use both: `build_instance` for the namespace object plus `register()` for any extra setup.

### NativeRegistrar

```rust
// crates/runtime/src/core/plugin.rs
pub struct NativeRegistrar {
    // Each entry is a closure that, given a scope + the namespace object,
    // installs one function (or runs setup). No live V8 scope is stored.
    pub(crate) entries: Vec<(&'static str, Box<dyn Fn(&mut v8::PinScope, v8::Local<v8::Object>)>)>,
}

impl NativeRegistrar {
    /// Register a native function as `env.{namespace}.{name}`.
    /// Callback signature:
    ///   fn(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue)
    pub fn add<F>(&mut self, name: &'static str, callback: F)
    where F: v8::MapFnTo<v8::FunctionCallback> + 'static;

    /// Run an arbitrary setup closure once during build_env_object, with the
    /// active scope + the namespace object. For side-effect setup beyond a
    /// single function — e.g. installing globals or auto-tx hooks. `name` is
    /// for debugging only.
    pub fn add_setup<F>(&mut self, name: &'static str, setup: F)
    where F: Fn(&mut v8::PinScope, v8::Local<v8::Object>) + 'static;
}
```

It collects closures rather than holding a scope, which avoids storing V8 types and the borrow conflicts that would create.

### How callbacks access everything they need

No context object is passed. The V8 scope **is** the context:

```rust
fn get_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    rv: v8::ReturnValue,
) {
    // app_id — from the isolate scope slot's env vars (set before dispatch).
    let state = scope.get_slot::<SharedState>().unwrap().clone();
    let app_id = state.borrow().env_vars.get("APP_ID").cloned();

    // Per-thread resource (pool / backend) — from thread_local!.
    KV_BACKEND.with(|b| { /* read / write */ });

    // Arguments — from V8.
    let key = args.get(0).to_rust_string_lossy(scope);
}
```

```
What the callback needs:       Where it gets it:
  app_id                        scope slot → SharedState.env_vars["APP_ID"]
  per-thread resource (pool)    thread_local!  (lazily initialized)
  function arguments            args (from V8)
  config (db_url, etc.)         self.<field>   (immutable, Send + Sync)
```

For a v8_class plugin (`plugin-db`), `app_id` is captured once at `build_instance` time (from `SharedState.env_vars["APP_ID"]`) and stamped onto the `Db` instance's internal field — the methods read `&self.app_id` rather than re-fetching it per call.

### Plugin state: thread_local, lazily initialized

Plugins hold only immutable config in the struct; per-thread resources live in `thread_local!`, initialized lazily on first use (there is no `init()` hook):

```rust
// plugin-db pattern (simplified)
pub struct DbPlugin { url: String }   // immutable, Send + Sync

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str { "db" }

    fn register(&self, r: &mut NativeRegistrar) {
        // Poison the per-thread URL so the lazy pool init can find it.
        // Idempotent: register() may fire repeatedly on the same thread.
        ctx_mut(|c| { if c.set_db_url(&self.url) { c.clear_pool(); } });
        // Extra setup beyond the v8_class methods: auto-tx query()/mutation() globals.
        r.add_setup("install_auto_tx_globals", |scope, _ns| {
            orchestrator::auto_tx::install_auto_tx_globals(scope);
        });
    }

    fn build_instance<'s>(&self, scope: &mut v8::PinScope<'s, '_>, app_id: &str)
        -> Option<v8::Local<'s, v8::Object>>
    {
        // Mint the `Db` v8_class instance; its `find`/`insert`/`update`/…
        // are #[v8_method]s, not NativeRegistrar callbacks.
        v8_classes::db::mint_db(scope, app_id)
    }
}
```

The connection pool is built on the first DB callback via `init_pool_async()` / `ensure_pool` (reading the URL poisoned in `register`), then reused for the thread's lifetime — surviving isolate eviction.

### How the runtime assembles `env`

```rust
// Sketch — real impl: crates/runtime/src/core/plugin.rs::build_env_object
fn build_env_object(
    scope: &mut v8::PinScope,
    plugins: &[Arc<dyn NativePlugin>],
    env_json: &str,            // { vars, secrets } — the per-app scalar env
) -> v8::Global<v8::Object> {
    let env_obj = v8::Object::new(scope);
    // 1. Layer scalar env: vars first, then secrets (secrets win on collision).

    // 2. Overlay each plugin namespace.
    for plugin in plugins {
        let mut registrar = NativeRegistrar::new();
        plugin.register(&mut registrar);

        // build_instance (Some → v8_class instance) OR a fresh plain object.
        let app_id = /* SharedState.env_vars["APP_ID"] */;
        let ns_obj = plugin.build_instance(scope, &app_id)
            .unwrap_or_else(|| v8::Object::new(scope));
        for (_name, apply) in &registrar.entries {
            apply(scope, ns_obj);                  // add() installs fns; add_setup() runs setup
        }
        let key = v8::String::new(scope, plugin.namespace()).unwrap();
        env_obj.set(scope, key.into(), ns_obj.into());
    }

    // 3. Shallow Object.freeze(env) — user code can't reassign env.db / add env.foo.
    //    (Namespace sub-objects keep their own-property methods; replacing them
    //    requires reassigning through the frozen parent, which fails.)
    freeze(scope, env_obj);
    v8::Global::new(scope, env_obj)
}
```

A second plugin claiming a namespace already taken panics — that's a platform misconfiguration, not a runtime-recoverable state.

## Lifecycle

```
Worker starts
  │
  ├─ 1. Construct plugins once, wrap in Arc<dyn NativePlugin>:
  │       Runtime::builder()
  │         .plugin(DbPlugin::new(&config.db_url))
  │         .plugin(StoragePlugin::local(&config.storage_root))
  │         .plugin(KvPlugin::in_memory())
  │
  │    ┌─── per Runtime / isolate (multi-tenant: one per app on the thread) ───┐
  │    │                                                                       │
  │    │ 2. build_env_object runs:                                             │
  │    │      register() per plugin   → idempotent thread-local poisoning      │
  │    │      build_instance() per plugin → e.g. mint the `Db` v8_class        │
  │    │      env frozen                                                       │
  │    │                                                                       │
  │    │   ┌─── per request ─────────────────────────────────────────────┐    │
  │    │   │ 3. Worker sets env_vars["APP_ID"] for the dispatch           │    │
  │    │   │ 4. V8 runs app code → env.db.find(...) → #[v8_method]         │    │
  │    │   │      → reads &self.app_id (stamped at build_instance)         │    │
  │    │   │      → ensure_pool() (lazy, first call) → thread_local pool   │    │
  │    │   │      → queries Postgres                                       │    │
  │    │   └───────────────────────────────────────────────────────────────┘  │
  │    │                                                                       │
  │    │ 5. Isolate destroyed (eviction / hot deploy).                         │
  │    │    Thread-local pools SURVIVE — they're per-thread, reused.           │
  │    └───────────────────────────────────────────────────────────────────────┘
  │
  └─ Worker shutdown — no plugin hook; thread-locals drop with the thread.
```

Key facts: `register()` runs per Runtime (idempotently). Thread-local resources are created lazily on first callback and live for the thread's lifetime, reused across isolates. There is **no** `init`/`shutdown` lifecycle hook.

## Plugin implementations

### plugin-db (`crates/plugin-db`)

`env.db` is a `Db` `#[v8_class]` instance, not a bag of flat callbacks.

```rust
pub struct DbPlugin { url: String }

impl DbPlugin {
    pub fn new(url: impl Into<String>) -> Self { Self { url: url.into() } }
}

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str { "db" }
    fn register(&self, r: &mut NativeRegistrar) {
        ctx_mut(|c| { if c.set_db_url(&self.url) { c.clear_pool(); } });
        r.add_setup("install_auto_tx_globals", |scope, _| {
            orchestrator::auto_tx::install_auto_tx_globals(scope);
        });
    }
    fn build_instance<'s>(&self, scope: &mut v8::PinScope<'s, '_>, app_id: &str)
        -> Option<v8::Local<'s, v8::Object>>
    { v8_classes::db::mint_db(scope, app_id) }
}
```

CRUD (`find`, `insert`, `update`, `delete`, `count`, `aggregate`, `distinct`, `upsert`, `search`, `near`, …), `collection(name)`, `registerModel`, transactions, and subscriptions are `#[v8_method]`s on the `Db` / `Collection` v8_classes (`src/v8_classes/`). See `docs/reference/db.md` and `crates/plugin-db/src/v8_classes/`.

### plugin-kv (`crates/plugin-kv`)

```rust
pub struct KvPlugin { backend: Arc<dyn Backend> }

impl KvPlugin {
    pub fn in_memory() -> Self;                       // dev default — no cross-worker state
    pub fn new() -> Self;                             // = in_memory()
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self;  // Redis, etc.
}

thread_local! { static KV_BACKEND: RefCell<Option<Arc<dyn Backend>>> = ...; }

impl NativePlugin for KvPlugin {
    fn namespace(&self) -> &str { "kv" }
    fn register(&self, r: &mut NativeRegistrar) {
        KV_BACKEND.with(|c| *c.borrow_mut() = Some(Arc::clone(&self.backend)));
        r.add("get", callbacks::get);
        r.add("set", callbacks::set);
        r.add("delete", callbacks::delete);
        r.add("incr", callbacks::incr);
        r.add("list", callbacks::list);
    }
}
```

### plugin-storage (`crates/plugin-storage`)

```rust
pub struct StoragePlugin { backend: Arc<dyn Backend> }

impl StoragePlugin {
    pub fn local(path: impl Into<PathBuf>) -> Self;          // filesystem (dev default)
    pub fn new(path: impl Into<PathBuf>) -> Self;            // = local()
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self;  // S3 / R2, etc.
}

thread_local! { static STORAGE_BACKEND: RefCell<Option<Arc<dyn Backend>>> = ...; }

impl NativePlugin for StoragePlugin {
    fn namespace(&self) -> &str { "storage" }
    fn register(&self, r: &mut NativeRegistrar) {
        STORAGE_BACKEND.with(|c| *c.borrow_mut() = Some(Arc::clone(&self.backend)));
        r.add("put", callbacks::put);
        r.add("get", callbacks::get);
        r.add("delete", callbacks::delete);
        r.add("list", callbacks::list);
    }
}
```

### Auth is NOT a plugin

`env.auth.*` is **not** a `NativePlugin`. It's installed directly by the runtime from `crates/runtime/src/auth.rs` and exposes `getUser()` / `requireUser()` — readers of the gateway-injected user context. There is no `plugin-auth` crate, and no `hash`/`signJwt` native ops (auth crypto lives in `@zeroship/*` npm packages over WebCrypto).

## Worker / CLI assembles plugins

The `Runtime` builder takes plugins via `.plugin(p)` (one) or `.plugins(vec)` (many); internally `Vec<Arc<dyn NativePlugin>>`.

```rust
// crates/worker/src/cache.rs (multi-tenant worker — DB only)
let plugins = vec![Arc::new(zeroship_plugin_db::DbPlugin::new(url)) as Arc<dyn NativePlugin>];

// crates/cli/src/main.rs (single-tenant `zeroship serve` — DB + storage + KV)
plugins.push(Arc::new(DbPlugin::new(url)));
plugins.push(Arc::new(StoragePlugin::new(storage_root.clone())));
plugins.push(Arc::new(KvPlugin::with_backend(redis_backend)));   // or KvPlugin::in_memory()
```

## Crate structure

```
crates/
├── runtime/              Kernel — V8, Web APIs, NativePlugin trait, env auth
│   ├── src/core/plugin.rs   NativePlugin trait + NativeRegistrar + build_env_object
│   ├── src/core/runtime.rs  Runtime builder — .plugin() / .plugins()
│   ├── src/core/init.rs     env.* assembly + the `zeroship` facade module
│   └── src/auth.rs          env.auth.getUser / requireUser (runtime-internal, not a plugin)
│
├── runtime-macros/      #[v8_class] / #[v8_method] proc macros
│
├── plugin-db/           env.db.* — Db/Collection v8_classes
│   ├── src/lib.rs           DbPlugin (build_instance mints Db)
│   ├── src/v8_classes/      Db, Collection, … #[v8_class] surfaces
│   ├── src/crud/            CRUD dispatch (encryption/mask/unmask passes)
│   ├── src/query.rs         filter → SQL builder
│   └── src/orchestrator/    register_model (schema validate + apply + auto-tx)
│
├── plugin-kv/           env.kv.* — KvPlugin + Backend (InMemory / Redis)
│   ├── src/lib.rs
│   ├── src/callbacks.rs     get/set/delete/incr/list
│   └── src/backend/
│
├── plugin-storage/      env.storage.* — StoragePlugin + Backend (LocalFs / S3)
│   ├── src/lib.rs
│   ├── src/callbacks.rs     put/get/delete/list
│   └── src/backend/
│
├── compio-postgres/     Postgres driver (compio-native; used by plugin-db)
│
├── worker/              Multi-tenant V8-per-thread worker; assembles plugins
└── cli/                 `zeroship serve` single-tenant; assembles plugins
```

### Dependency graph

```
runtime ← runtime-macros (proc-macro dep, build-time)
runtime ← plugin-db ← worker, cli
runtime ← plugin-kv ← cli
runtime ← plugin-storage ← cli
compio-postgres ← plugin-db

runtime does NOT depend on any plugin. Plugins depend on runtime (for the trait
+ #[v8_class] macro). worker/cli depend on the plugins they assemble.
```

## Security

### Frozen namespace

```javascript
env.db = {};              // TypeError — env is frozen
env.foo = {};             // TypeError — env is frozen (not extensible)
delete env.db;            // TypeError — non-configurable
```

`build_env_object` shallow-`Object.freeze`s `env`. Namespace sub-objects (`env.db`, `env.kv`) keep their methods as own properties attached at build time; user code can't reassign or delete `env.db`, nor add `env.foo`. (Reassigning a method *through* the frozen parent fails because the parent is frozen.)

### Callback safety

Every callback must:
- Read `app_id` from the scope slot's env vars (or `&self.app_id` for v8_class plugins) — never trust V8 args for tenant identity.
- Scope all operations to the app's schema / key-prefix / bucket-prefix.
- Validate all V8 string input (collection names, keys, filters, operators).
- Use parameterized SQL — never interpolate (plugin-db builds parameterized queries in `query.rs`).

### Collection / identifier validation (plugin-db)

Collection and field names are validated against a reserved-name list + an allowed-character set before they reach SQL. Reserved prefixes (`_*`, `__zs_*`, `__zeroship_*`, `sqlite_*`), reserved suffixes (`_masked`), and the platform system-field names are refused at schema-registration time (`crates/plugin-db/src/query.rs` — `RESERVED_NAMES` + `validate_field_name`).

## Metering

Metering is **not** part of the plugin callback surface today. There is no `state.meter.increment(...)` call inside plugin callbacks. Billing counters are a separate concern: the control plane's metering subsystem (`crates/control/src/metering.rs`) and the planned `env.meter.*` namespace (not yet implemented). When `env.meter` lands it will be a plugin like the others. Do not assume a meter handle on `SharedState`.

## Adding a new plugin

1. Create `crates/plugin-foo/`, depend on `zeroship-runtime`.
2. Implement `NativePlugin`. Pick a mechanism:
   - **Flat callbacks** (like kv/storage): `register()` + `r.add("bar", callbacks::bar)`.
   - **v8_class** (like db): `build_instance()` returning a `#[v8_class]` instance.
3. Hold immutable config in the struct; per-thread resources in `thread_local!`, initialized lazily on first callback (no `init()` hook).
4. Add to the worker / CLI assembly via `.plugin(FooPlugin::new(...))`.
5. Publish the SDK wrapper: `@zeroship/foo` on npm.
6. Done — no changes to the runtime or existing plugins.

```rust
// crates/plugin-foo/src/lib.rs — flat-callback flavor
pub struct FooPlugin;

impl NativePlugin for FooPlugin {
    fn namespace(&self) -> &str { "foo" }
    fn register(&self, r: &mut NativeRegistrar) {
        r.add("bar", callbacks::bar);
    }
}

// JS:  await env.foo.bar("hello")     (env = 2nd fetch arg / `env` export of `zeroship`)
// SDK: import { foo } from "@zeroship/foo"
```
