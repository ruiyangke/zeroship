# Runtime + Storage + Bytecode Cache Design

**Goal:** Refactor `isolate_v8` into a complete runtime that loads apps from filesystem with V8 bytecode caching. No HTTP, no API — just compile, store, run.

**Core interface:**

```rust
// Compile
let artifact = appbase_compiler::compile(&source, Target::Node);

// Store
let store = AppStorage::new("data/apps/my-app");
store.deploy(&artifact.server, artifact.client.as_deref())?;

// Load + Run
let mut isolate = Isolate::from_path(store.current_server_js())?;
let result = isolate.execute_request(rpc_json)?;
```

---

## Storage Layout

```
data/apps/{app_id}/
  source/
    app.tsx                    ← original source (kept for AI editing)
  builds/
    v1/
      server.js                ← bundled output from SWC
      server.js.cache          ← V8 bytecode cache
      client/
        index.html             ← client bundle
      meta.json                ← { version, build_time_ms, source_hash }
    v2/
      ...
  current → builds/v2          ← symlink to active version
```

## Changes to isolate_v8

### 1. AppStorage (new module: `storage.rs`)

```rust
pub struct AppStorage {
    base_dir: PathBuf,
}

impl AppStorage {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self;
    pub fn deploy(&self, server_js: &str, client_html: Option<&[u8]>) -> Result<u64, StorageError>;
    pub fn current_server_js(&self) -> PathBuf;
    pub fn current_client_dir(&self) -> PathBuf;
    pub fn current_version(&self) -> Option<u64>;
    pub fn rollback(&self, version: u64) -> Result<(), StorageError>;
    pub fn versions(&self) -> Vec<u64>;
    pub fn source_path(&self) -> PathBuf;
    pub fn save_source(&self, filename: &str, content: &str) -> Result<(), StorageError>;
}
```

### 2. Bytecode cache (modify `isolate.rs` + `concurrent.rs`)

Replace `v8::String → v8::Script::compile` with:

```rust
fn compile_with_cache(scope, js_path, cache_path) -> v8::Local<v8::Script> {
    let source = std::fs::read_to_string(js_path)?;
    let v8_src = v8::String::new(scope, &source)?;

    // Try bytecode cache
    if let Ok(bytecode) = std::fs::read(cache_path) {
        let cached = v8::script_compiler::CachedData::new(&bytecode);
        let source = v8::script_compiler::Source::new(v8_src, Some(cached));
        return v8::script_compiler::compile(scope, source, ConsumeCodeCache);
    }

    // Compile from source, save cache
    let source = v8::script_compiler::Source::new(v8_src, None);
    let script = v8::script_compiler::compile(scope, source, EagerCompile);
    if let Some(cache) = v8::script_compiler::create_code_cache(script) {
        std::fs::write(cache_path, cache)?;
    }
    script
}
```

### 3. Isolate::from_path (new constructor)

```rust
impl Isolate {
    /// Load from filesystem with bytecode caching.
    pub fn from_path(js_path: &Path) -> Self {
        let cache_path = js_path.with_extension("js.cache");
        // ... create isolate, compile_with_cache, setup globals
    }
}
```

### 4. ConcurrentIsolate::from_path

Same pattern for the concurrent model.

## Memory lifecycle

```
Deploy:
  source (10MB) → SWC compile → server.js (10MB) on disk
  → V8 compile → bytecode (20MB) → save to .cache file
  → drop source string from memory

First request (no cache):
  read server.js from disk → compile → save .cache → drop source
  V8 heap: ~20MB bytecodeonly

First request (with cache):
  read server.js + .cache from disk → load bytecode → drop source
  V8 heap: ~20MB, startup: <1ms

Eviction:
  drop isolate → free 20MB
  .cache file persists on disk

Re-warm after eviction:
  read .cache → load bytecode → <1ms startup
```

## What NOT to change

- RPC dispatch, event loop, fetch, kv, timers — all unchanged
- ConcurrentIsolate event loop — unchanged
- Server crate — will adapt to call from_path instead of passing strings
