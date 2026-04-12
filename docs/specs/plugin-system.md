# Plugin System

## Overview

The runtime is a kernel. Plugins are drivers. The runtime provides V8, Web APIs (fetch, crypto, console, timers), and a plugin registration API. Platform features (database, auth, storage, KV) are plugins that register native functions on the `appbase.*` global namespace.

```
Runtime (kernel):
  V8 isolate, module loading, event loop, Web APIs
  NativePlugin trait — the extension point

Plugins (drivers):
  plugin-db      → appbase.db.*
  plugin-auth    → appbase.auth.*
  plugin-storage → appbase.storage.*
  plugin-kv      → appbase.kv.*

Worker (assembler):
  Creates Runtime with chosen plugins
  Each plugin gets its own namespace on appbase.*
```

## Design

### NativePlugin trait

```rust
/// A native extension that registers functions on the appbase.* global.
pub trait NativePlugin: Send {
    /// Namespace under appbase.* (e.g., "db", "auth", "storage", "kv").
    /// Must be a valid JS identifier: lowercase, alphanumeric, no dots.
    fn namespace(&self) -> &str;

    /// Register native functions. Called once per V8 isolate at creation time.
    /// The registrar provides the V8 scope and the namespace object.
    fn register(&self, registrar: &mut NativeRegistrar);

    /// Optional: called when the isolate is about to be destroyed.
    /// Use for cleanup (close connections, flush buffers).
    fn on_destroy(&self) {}

    /// Optional: human-readable name for logging/debugging.
    fn name(&self) -> &str { self.namespace() }
}
```

### NativeRegistrar

```rust
/// Passed to plugins during registration. Provides methods to add
/// native functions to the plugin's namespace object.
pub struct NativeRegistrar<'a, 'b> {
    scope: &'a mut v8::PinScope<'b>,
    namespace_obj: v8::Local<'b, v8::Object>,
    state: SharedState,
}

impl<'a, 'b> NativeRegistrar<'a, 'b> {
    /// Register a synchronous native function.
    /// Accessible as appbase.{namespace}.{name}()
    pub fn add_sync(
        &mut self,
        name: &str,
        callback: impl Fn(&mut v8::PinScope, v8::FunctionCallbackArguments, v8::ReturnValue) + 'static,
    );

    /// Register an async native function that returns a Promise.
    /// Accessible as await appbase.{namespace}.{name}()
    pub fn add_async(
        &mut self,
        name: &str,
        callback: impl Fn(&mut v8::PinScope, v8::FunctionCallbackArguments) -> Pin<Box<dyn Future<Output = Result<String, String>>>> + 'static,
    );

    /// Register a constant value.
    /// Accessible as appbase.{namespace}.{name}
    pub fn add_value(&mut self, name: &str, value: v8::Local<'b, v8::Value>);

    /// Get the shared runtime state (for accessing app_id, meter, etc.)
    pub fn state(&self) -> &SharedState;
}
```

### How the runtime uses plugins

```rust
// runtime/src/runtime.rs

pub struct Runtime {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    plugins: Vec<Box<dyn NativePlugin>>,
    // ... existing fields
}

impl Runtime {
    pub fn new(
        modules: Vec<ModuleEntry>,
        plugins: Vec<Box<dyn NativePlugin>>,
        env_vars: HashMap<String, String>,
        cpu_limit: Option<Duration>,
        wall_timeout: Option<Duration>,
    ) -> Self {
        // ... existing V8 setup ...

        // Create appbase global namespace
        enter_v8!(self, |scope| {
            let global = scope.get_current_context().global(scope);
            let appbase = v8::Object::new(scope);

            // Register each plugin
            for plugin in &plugins {
                let ns_name = plugin.namespace();
                let ns_obj = v8::Object::new(scope);
                let mut registrar = NativeRegistrar {
                    scope,
                    namespace_obj: ns_obj,
                    state: self.state.clone(),
                };
                plugin.register(&mut registrar);

                // Freeze the namespace (prevent modification)
                freeze_object(scope, ns_obj);
                let key = v8::String::new(scope, ns_name).unwrap();
                appbase.set(scope, key.into(), ns_obj.into());
            }

            // Freeze appbase itself
            freeze_object(scope, appbase);
            let key = v8::String::new(scope, "appbase").unwrap();
            global.set(scope, key.into(), appbase.into());
        });

        Self { isolate, context, plugins, ... }
    }
}
```

## Plugin implementations

### plugin-db

```rust
// crates/plugin-db/src/lib.rs

use appbase_runtime::{NativePlugin, NativeRegistrar};
use appbase_pg::Pool;

pub struct DbPlugin {
    db_url: String,
}

impl DbPlugin {
    pub fn new(db_url: &str) -> Self {
        Self { db_url: db_url.to_string() }
    }
}

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str { "db" }
    fn name(&self) -> &str { "database" }

    fn register(&self, r: &mut NativeRegistrar) {
        // Store db_url in isolate state for lazy pool creation
        // (pool is Rc-based, created per-thread on first use)

        r.add_async("find", callbacks::find);
        r.add_async("findOne", callbacks::find_one);
        r.add_async("insert", callbacks::insert);
        r.add_async("insertMany", callbacks::insert_many);
        r.add_async("updateOne", callbacks::update_one);
        r.add_async("updateMany", callbacks::update_many);
        r.add_async("deleteOne", callbacks::delete_one);
        r.add_async("deleteMany", callbacks::delete_many);
        r.add_async("count", callbacks::count);
        r.add_async("aggregate", callbacks::aggregate);
        r.add_async("distinct", callbacks::distinct);
        r.add_sync("registerModel", callbacks::register_model);
    }
}
```

### plugin-auth

```rust
// crates/plugin-auth/src/lib.rs

pub struct AuthPlugin {
    jwt_secret: String,
}

impl NativePlugin for AuthPlugin {
    fn namespace(&self) -> &str { "auth" }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add_async("hash", callbacks::hash);             // argon2 hash
        r.add_async("verifyHash", callbacks::verify_hash); // argon2 verify
        r.add_sync("signJwt", callbacks::sign_jwt);        // sync — fast
        r.add_sync("verifyJwt", callbacks::verify_jwt);    // sync — fast
    }
}
```

### plugin-storage

```rust
// crates/plugin-storage/src/lib.rs

pub struct StoragePlugin {
    backend: Box<dyn StorageBackend>,  // LocalFs or S3
}

impl NativePlugin for StoragePlugin {
    fn namespace(&self) -> &str { "storage" }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add_async("put", callbacks::put);
        r.add_async("get", callbacks::get);
        r.add_async("delete", callbacks::delete);
        r.add_async("exists", callbacks::exists);
    }
}
```

### plugin-kv

```rust
// crates/plugin-kv/src/lib.rs

pub struct KvPlugin {
    // v1: in-memory HashMap per isolate
    // v2: Redis
}

impl NativePlugin for KvPlugin {
    fn namespace(&self) -> &str { "kv" }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add_async("get", callbacks::get);
        r.add_async("set", callbacks::set);
        r.add_async("delete", callbacks::delete);
        r.add_async("list", callbacks::list);
    }
}
```

## Crate structure

```
crates/
├── runtime/              Kernel — V8, event loop, Web APIs, NativePlugin trait
│   ├── src/plugin.rs     NativePlugin trait + NativeRegistrar
│   ├── src/runtime.rs    Runtime::new() accepts plugins
│   └── src/init.rs       setup_globals (Web APIs + plugin registration)
│
├── plugin-db/            Database plugin
│   ├── src/lib.rs        DbPlugin: impl NativePlugin
│   ├── src/callbacks.rs  find, insert, update, delete V8 callbacks
│   ├── src/query.rs      filter → parameterized SQL
│   ├── src/validate.rs   model validation
│   ├── src/migrate.rs    schema diffing + ALTER TABLE
│   └── Cargo.toml        deps: runtime, pg
│
├── plugin-auth/          Auth plugin
│   ├── src/lib.rs        AuthPlugin: impl NativePlugin
│   ├── src/callbacks.rs  hash, verifyHash, signJwt, verifyJwt
│   └── Cargo.toml        deps: runtime, argon2, jsonwebtoken
│
├── plugin-storage/       Storage plugin
│   ├── src/lib.rs        StoragePlugin: impl NativePlugin
│   ├── src/callbacks.rs  put, get, delete
│   ├── src/backend.rs    StorageBackend trait (LocalFs, S3)
│   └── Cargo.toml        deps: runtime
│
├── plugin-kv/            KV plugin
│   ├── src/lib.rs        KvPlugin: impl NativePlugin
│   ├── src/callbacks.rs  get, set, delete, list
│   └── Cargo.toml        deps: runtime
│
├── worker/               Assembles plugins
│   ├── src/main.rs       creates Runtime with plugins
│   └── Cargo.toml        deps: runtime, plugin-db, plugin-auth, ...
│
└── pg/                   Postgres driver (used by plugin-db)
```

### Dependency graph

```
runtime ← plugin-db ← worker
        ← plugin-auth ← worker
        ← plugin-storage ← worker
        ← plugin-kv ← worker
        ← pg ← plugin-db

runtime does NOT depend on any plugin.
Plugins depend on runtime (for the NativePlugin trait).
Worker depends on all plugins it wants.
pg is used by plugin-db, not by runtime.
```

## Worker assembles plugins

```rust
// worker/src/main.rs

fn create_runtime(modules: Vec<ModuleEntry>, config: &WorkerConfig) -> Runtime {
    let mut plugins: Vec<Box<dyn NativePlugin>> = Vec::new();

    // Always include
    plugins.push(Box::new(DbPlugin::new(&config.db_url)));
    plugins.push(Box::new(AuthPlugin::new(&config.jwt_secret)));
    plugins.push(Box::new(KvPlugin::new()));

    // Optional — based on config
    if let Some(storage_path) = &config.storage_path {
        plugins.push(Box::new(StoragePlugin::new_local(storage_path)));
    }

    Runtime::new(modules, plugins, HashMap::new(), config.cpu_limit, config.wall_timeout)
}
```

## Security

### Namespace isolation

Each plugin gets its own frozen namespace. Plugins cannot:
- Access other plugins' namespaces
- Modify the `appbase` global after registration
- Access V8 internals outside of their callbacks

```javascript
// User code cannot modify primitives:
appbase.db.find = () => "hacked";    // TypeError: Cannot assign to read-only property
appbase.db.drop = () => {};           // TypeError: Cannot add property
delete appbase.db;                    // TypeError: Cannot delete property
appbase.evil = {};                    // TypeError: Cannot add property
```

### Plugin callback safety

All plugin callbacks:
- Receive the app_id from RuntimeState (set per-request)
- Must scope operations to the app (e.g., SET search_path)
- Must use the Meter trait for billing
- Must validate all input from V8 (strings, not trusted)

### Plugin state

Plugins may need per-thread state (connection pools, caches). This is stored in RuntimeState via a typed slot:

```rust
impl RuntimeState {
    /// Get plugin-specific state by type.
    pub fn plugin_state<T: 'static>(&self) -> Option<&T>;

    /// Set plugin-specific state.
    pub fn set_plugin_state<T: 'static>(&mut self, state: T);
}

// In DbPlugin:
fn register(&self, r: &mut NativeRegistrar) {
    // Create per-thread pool, store in state
    let pool = Pool::connect_lazy(&self.db_url, 8);
    r.state().borrow_mut().set_plugin_state(pool);
    // ...
}

// In callbacks:
fn find_callback(scope: &mut v8::PinScope, args: ...) {
    let state = get_state(scope);
    let pool: &Pool = state.plugin_state::<Pool>().unwrap();
    // use pool
}
```

## Metering

Plugins access the meter through the registrar's state:

```rust
fn find_callback(scope: &mut v8::PinScope, args: ...) {
    let state = get_state(scope);

    // Execute query
    let rows = pool.query(&sql, &params).await?;

    // Record billing metrics
    state.meter.increment("db.reads", 1);
    state.meter.increment("db.rows_read", rows.len() as u64);
}
```

Each plugin defines its own metric names (namespaced: `db.*`, `auth.*`, `storage.*`). The billing system aggregates all metrics regardless of which plugin produced them.

## Adding a new plugin

1. Create crate: `crates/plugin-foo/`
2. Implement `NativePlugin` trait
3. Add to worker's `Cargo.toml` and `create_runtime()`
4. Publish corresponding npm SDK: `@appbase/foo`
5. Document: `appbase.foo.*` primitives + `@appbase/foo` SDK API

No changes to the runtime crate. No changes to existing plugins.

```rust
// Example: crates/plugin-email/src/lib.rs

pub struct EmailPlugin {
    api_key: String,
    provider: String,  // "resend", "sendgrid"
}

impl NativePlugin for EmailPlugin {
    fn namespace(&self) -> &str { "email" }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add_async("send", callbacks::send);
    }
}

// callback uses fetch() internally — no new native capability needed
// but registered as a native plugin for metering + config injection
```
