# Node.js Compatibility

Zeroship supports a narrow set of Node APIs directly in the runtime and relies on the Vite plugin for the rest of the compatibility story.

## Runtime-native `node:` modules

The runtime currently registers these synthetic modules in [crates/runtime/src/core/native_modules.rs](../../crates/runtime/src/core/native_modules.rs):

- `node:async_hooks`
- `node:buffer`
- `node:crypto`
- `node:zlib`
- `node:os`
- `node:path`
- `node:util`

Their implementations live under [crates/runtime/src/node/mod.rs](../../crates/runtime/src/node/mod.rs).

## Build-time compatibility

The Vite plugin compatibility layer is implemented in [sdks/vite-plugin/src/node-compat.ts](../../sdks/vite-plugin/src/node-compat.ts).

That layer currently combines:

- direct pass-through for a small runtime-native set
- custom polyfills for modules such as `node:timers/promises`, `node:module`, and `node:process`
- `unenv`-driven polyfills for additional packages and built-ins

## Practical boundary

This is not full Node.js. Prefer Web-standard APIs when possible, and only rely on Node compatibility that is explicitly implemented in the runtime or the Vite plugin sources above.
