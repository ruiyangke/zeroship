# Plugin System

## Overview

The runtime is a kernel. Plugins are drivers. The runtime provides V8, Web APIs (fetch, crypto, console, timers), and a plugin registration API. Platform features (database, auth, storage, KV) are plugins that register native functions as `env.*` namespaces. Creator code reaches them via `env.<namespace>.*` — `env` is the 2nd arg to `fetch(req, env, ctx)` and the `env` named export of the `zeroship` module.

## Design

### NativePlugin trait

```rust
pub trait NativePlugin: Send + Sync {
    /// Namespace under env.* (e.g., "db", "auth", "storage", "kv").
    fn namespace(&self) -> &str;

    /// Human-readable name for logging.
    fn name(&self) -> &str { self.namespace() }

    /// Called once per worker thread. Set up thread-local resources
    /// (connection pools, caches). Async — can connect to databases.
    async fn init(&self, config: &Arc<WorkerConfig>);

    /// Called once per V8 isolate. Register functions on env.{namespace}.
    fn register(&self, registrar: &mut NativeRegistrar);

    /// Called on worker shutdown. Close connections, flush buffers.
    async fn shutdown(&self) {}
}
```

Three hooks. No context object. No per-request hooks.

### NativeRegistrar

```rust
pub struct NativeRegistrar<'a, 'b> {
    scope: &'a mut v8::PinScope<'b>,
    namespace_obj: v8::Local<'b, v8::Object>,
}

impl NativeRegistrar {
    /// Register a native function as env.{namespace}.{name}
    pub fn add(&mut self, name: &str, callback: v8::FunctionCallback);
}
```

That's it. One method. Adds a V8 function to the namespace object.

### How callbacks access everything they need

No context passing. No dependency injection. The V8 scope is the context:

```rust
fn find_one_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    rv: v8::ReturnValue,
) {
    // App ID — from RuntimeState (set by worker before each dispatch)
    let state: SharedState = scope.get_slot::<SharedState>().unwrap().clone();
    let app_id = state.borrow().app_id.clone().unwrap();

    // Meter — from RuntimeState
    state.borrow().meter.increment("db.reads", 1);

    // Pool — from thread_local (set by plugin's init)
    DB_POOL.with(|p| { /* query */ });

    // Args — from V8
    let collection = args.get(0).to_rust_string_lossy(scope);
    let filter = args.get(1).to_rust_string_lossy(scope);
}
```

```
What the callback needs:       Where it gets it:
  app_id                        scope → RuntimeState.app_id
  meter                         scope → RuntimeState.meter
  connection pool               thread_local! (set in init)
  function arguments            args (from V8)
  config (db_url, etc.)         self.config (immutable, Send+Sync)
```

### Plugin state: thread_local

Plugins store per-thread resources (connection pools, caches) in `thread_local!`. The plugin struct itself is `Send + Sync` and holds only immutable config:

```rust
pub struct DbPlugin {
    db_url: String,     // immutable, Send + Sync
}

thread_local! {
    static DB_POOL: RefCell<Option<Pool>> = RefCell::new(None);
}

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str { "db" }

    async fn init(&self, config: &Arc<WorkerConfig>) {
        let pool = Pool::connect(&config.db_url, 8).await.unwrap();
        DB_POOL.with(|p| *p.borrow_mut() = Some(pool));
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("find", find_callback);
        r.add("insert", insert_callback);
        r.add("insertMany", insert_many_callback);
        r.add("update", update_callback);
        r.add("updateMany", update_many_callback);
        r.add("delete", delete_callback);
        r.add("deleteMany", delete_many_callback);
        r.add("count", count_callback);
        r.add("aggregate", aggregate_callback);
        r.add("distinct", distinct_callback);
        r.add("registerModel", register_model_callback);
    }

    async fn shutdown(&self) {
        DB_POOL.with(|p| p.borrow_mut().take()); // drop pool
    }
}
```

### How the runtime uses plugins

```rust
// Sketch — see `crates/runtime/src/core/plugin.rs::build_env_object`
// for the real implementation. Plugin namespaces are layered onto the
// per-app `env` object that user code receives as the 2nd arg to
// `fetch(req, env, ctx)` (and as the `env` named export of the
// `zeroship` module).
fn build_env_object(
    scope: &mut v8::PinScope,
    plugins: &[Arc<dyn NativePlugin>],
    env_json: &str,
) -> v8::Global<v8::Object> {
    let env_obj = v8::Object::new(scope);
    // ... merge `env_json` scalars (vars + secrets) onto `env_obj` ...

    for plugin in plugins {
        let mut registrar = NativeRegistrar::new();
        plugin.register(&mut registrar);

        // Plugins may ship a v8_class-backed instance via build_instance;
        // otherwise we allocate a plain object.
        let ns_obj = plugin
            .build_instance(scope, app_id)
            .unwrap_or_else(|| v8::Object::new(scope));
        for (_name, apply_fn) in &registrar.entries {
            apply_fn(scope, ns_obj);
        }

        let ns_key = v8::String::new(scope, plugin.namespace()).unwrap();
        env_obj.set(scope, ns_key.into(), ns_obj.into());
    }

    // Shallow Object.freeze on env — user code can't reassign env.db.
    freeze(scope, env_obj);
    v8::Global::new(scope, env_obj)
}
```

## Lifecycle

```
Worker starts
  │
  ├─ 1. Construct plugins (once)
  │     let db = DbPlugin::new(&config.db_url);
  │     let auth = AuthPlugin::new(&config.jwt_secret);
  │
  ├─ 2. init() per thread (once per worker thread)
  │     db.init(config).await     → Pool::connect, store in thread_local
  │     auth.init(config).await   → load JWT keys
  │
  │    ┌─── repeats per isolate ───────────────────────────────┐
  │    │                                                       │
  │    │ 3. Create V8 isolate                                  │
  │    │    register() called for each plugin                  │
  │    │    → env.db.find, .insert, ... added to env            │
  │    │    → env.auth.hash, .signJwt, ... added                │
  │    │    → env object frozen                                 │
  │    │                                                       │
  │    │   ┌─── repeats per request ──────────────────────┐    │
  │    │   │                                              │    │
  │    │   │ 4. Worker sets state.app_id = "app_abc"      │    │
  │    │   │ 5. V8 executes app code                      │    │
  │    │   │    → env.db.find() → callback                │    │
  │    │   │      → reads app_id from state               │    │
  │    │   │      → reads pool from thread_local          │    │
  │    │   │      → queries Postgres                      │    │
  │    │   │ 6. Worker clears state.app_id                │    │
  │    │   │                                              │    │
  │    │   └──────────────────────────────────────────────┘    │
  │    │                                                       │
  │    │ 7. Isolate destroyed (eviction / hot deploy)          │
  │    │    (pool and thread_locals survive — they're per-thread)│
  │    │                                                       │
  │    └───────────────────────────────────────────────────────┘
  │
  └─ 8. Worker shutdown
        db.shutdown().await     → drop pool
        auth.shutdown().await   → flush
```

Key insight: `init()` runs once per thread. `register()` runs once per isolate. Thread-local state (pools) survives isolate destruction — they're reused across isolates on the same thread.

## Plugin implementations

### plugin-db

```rust
pub struct DbPlugin { db_url: String }

thread_local! { static DB_POOL: RefCell<Option<Pool>> = RefCell::new(None); }

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str { "db" }

    async fn init(&self, config: &Arc<WorkerConfig>) {
        let pool = Pool::connect(&config.db_url, 8).await.unwrap();
        DB_POOL.with(|p| *p.borrow_mut() = Some(pool));
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("find", callbacks::find);
        r.add("insert", callbacks::insert);
        r.add("update", callbacks::update);
        r.add("updateMany", callbacks::update_many);
        r.add("delete", callbacks::delete);
        r.add("deleteMany", callbacks::delete_many);
        r.add("count", callbacks::count);
        r.add("aggregate", callbacks::aggregate);
        r.add("distinct", callbacks::distinct);
        r.add("registerModel", callbacks::register_model);
    }
}
```

### plugin-auth

```rust
pub struct AuthPlugin { jwt_secret: String }

impl NativePlugin for AuthPlugin {
    fn namespace(&self) -> &str { "auth" }

    async fn init(&self, _config: &Arc<WorkerConfig>) {}

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("hash", callbacks::hash);
        r.add("verifyHash", callbacks::verify_hash);
        r.add("signJwt", callbacks::sign_jwt);
        r.add("verifyJwt", callbacks::verify_jwt);
    }
}
```

### plugin-storage

```rust
pub struct StoragePlugin { backend: Arc<dyn StorageBackend> }

impl NativePlugin for StoragePlugin {
    fn namespace(&self) -> &str { "storage" }

    async fn init(&self, _config: &Arc<WorkerConfig>) {}

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("put", callbacks::put);
        r.add("get", callbacks::get);
        r.add("delete", callbacks::delete);
        r.add("exists", callbacks::exists);
    }
}
```

### plugin-kv

```rust
pub struct KvPlugin;

thread_local! { static KV_STORE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new()); }

impl NativePlugin for KvPlugin {
    fn namespace(&self) -> &str { "kv" }

    async fn init(&self, _config: &Arc<WorkerConfig>) {}

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("get", callbacks::get);
        r.add("set", callbacks::set);
        r.add("delete", callbacks::delete);
    }
}
```

## Worker assembles plugins

```rust
// worker/src/main.rs

fn create_runtime(modules: Vec<ModuleEntry>, config: &WorkerConfig) -> Runtime {
    let plugins: Vec<Box<dyn NativePlugin>> = vec![
        Box::new(DbPlugin::new(&config.db_url)),
        Box::new(AuthPlugin::new(&config.jwt_secret)),
        Box::new(StoragePlugin::new(&config.storage_path)),
        Box::new(KvPlugin::new()),
    ];

    Runtime::new(modules, &plugins, HashMap::new(), config.cpu_limit, config.wall_timeout)
}
```

## Crate structure

```
crates/
├── runtime/              Kernel — V8, Web APIs, NativePlugin trait
│   ├── src/plugin.rs     NativePlugin trait + NativeRegistrar
│   ├── src/runtime.rs    Runtime::new() accepts &[Box<dyn NativePlugin>]
│   └── src/init.rs       setup env.* namespaces from plugins
│
├── plugin-db/            Database
│   ├── src/lib.rs        DbPlugin
│   ├── src/callbacks.rs  V8 callbacks
│   ├── src/query.rs      filter → SQL
│   ├── src/validate.rs   schema validation
│   └── src/migrate.rs    auto-migration
│
├── plugin-auth/          Authentication
│   ├── src/lib.rs        AuthPlugin
│   └── src/callbacks.rs  hash, JWT
│
├── plugin-storage/       Object storage
│   ├── src/lib.rs        StoragePlugin
│   └── src/backend.rs    LocalFs / S3
│
├── plugin-kv/            Key-value
│   └── src/lib.rs        KvPlugin
│
├── worker/               Assembles everything
│   └── Cargo.toml        deps: runtime, plugin-db, plugin-auth, ...
│
└── pg/                   Postgres driver (used by plugin-db)
```

### Dependency graph

```
runtime  ← plugin-db ← worker
         ← plugin-auth ← worker
         ← plugin-storage ← worker
         ← plugin-kv ← worker
         ← pg ← plugin-db

runtime does NOT depend on any plugin.
Plugins depend on runtime (for the trait).
Worker depends on all plugins it needs.
```

## Security

### Frozen namespace

```javascript
env.db.find = () => "hacked";    // TypeError: read-only
env.db.evil = () => {};           // TypeError: not extensible
delete env.db;                    // TypeError: non-configurable
env.foo = {};                     // TypeError: frozen
```

`Object.freeze()` applied to `env` (Stage 1 of the macro-driven DB
namespace). Namespace sub-objects (`env.db`, `env.kv`, …) remain
unfrozen by reference, but their registered methods are attached as
own properties at build time — replacing them requires reassigning
through the frozen parent, which fails. User code cannot reassign or
delete `env.db` itself, nor add `env.foo`.

### Callback safety

Every callback must:
- Read `app_id` from `RuntimeState` (never trust V8 args for identity)
- Scope all operations to the app's schema
- Validate all V8 string input (collection names, filters, operators)
- Use parameterized SQL (never interpolate)
- Call `meter.increment()` for billing

### Collection name validation

```rust
fn validate_collection(name: &str, registered_models: &HashSet<String>) -> Result<()> {
    if !registered_models.contains(name) {
        return Err(DbError::new("UNKNOWN_COLLECTION", "not found"));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(DbError::new("INVALID_COLLECTION", "invalid name"));
    }
    Ok(())
}
```

## Metering

Plugins call `meter.increment()` via RuntimeState:

```rust
fn find_callback(scope: &mut v8::PinScope, args: ...) {
    let state = get_state(scope);

    // Execute query
    let rows = /* ... */;

    // Record billing
    state.borrow().meter.increment("db.reads", 1);
    state.borrow().meter.increment("db.rows_read", rows.len() as u64);
}
```

Metric names are namespaced by plugin: `db.*`, `auth.*`, `storage.*`, `kv.*`.

## Adding a new plugin

1. Create `crates/plugin-foo/`
2. Implement `NativePlugin` (namespace, init, register)
3. Add to `worker/Cargo.toml` and `create_runtime()`
4. Publish SDK: `@zeroship/foo` on npm
5. Done — no changes to runtime or existing plugins

```rust
// crates/plugin-foo/src/lib.rs
pub struct FooPlugin;

impl NativePlugin for FooPlugin {
    fn namespace(&self) -> &str { "foo" }
    async fn init(&self, _config: &Arc<WorkerConfig>) {}
    fn register(&self, r: &mut NativeRegistrar) {
        r.add("bar", callbacks::bar);
    }
}

// JS: await env.foo.bar("hello")           (env = 2nd arg to fetch / `env` named export of `zeroship`)
// SDK: import { foo } from "@zeroship/foo"
```
