# Bundle & Module System Design

**Date:** 2026-04-03
**Goal:** Support projects from single file to large codebases with npm deps via esbuild bundling.

## Pipeline

```
User source (TS/TSX/JS/JSX, any size, npm deps)
  → esbuild --bundle --format=esm (shell out, ~10ms)
  → Single optimized ESM
  → AppStorage.deploy()
  → V8 bytecode cache
  → Isolate loads
```

## Two deploy modes

1. **Source deploy** — platform runs esbuild on uploaded source directory
2. **Pre-bundled deploy** — user uploads single JS file (skip esbuild)

## Compiler crate changes

New `bundler.rs` module alongside existing SWC compiler:

```rust
pub struct BundleOptions {
    pub entry: String,
    pub minify: bool,
    pub sourcemap: bool,
    pub target: String,
    pub external: Vec<String>,
}

pub struct BundleResult {
    pub js: String,
    pub source_map: Option<String>,
    pub size_bytes: usize,
}

pub fn bundle(project_dir: &Path, options: &BundleOptions) -> Result<BundleResult, BundleError>;
pub fn detect_entry(project_dir: &Path) -> Option<String>;
pub fn esbuild_available() -> bool;
```

## esbuild flags

```
esbuild {entry} --bundle --format=esm --platform=neutral --target=es2022
  --tree-shaking=true --minify --sourcemap=external
  --conditions=workerd,worker --log-level=warning --write=false
```

## Entry point detection

1. package.json "main" or "module" field
2. src/index.ts → src/index.js → index.ts → index.js
3. src/server.ts → src/server.js → server.ts → server.js
4. Single .ts/.js file in root → use it

## What doesn't change

- V8 module loader, runtime APIs, event loop, benchmarks — unchanged
- Existing SWC compiler (lib.rs) — kept for full-stack React split mode
- AppStorage — already designed for this flow
