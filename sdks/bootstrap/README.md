# @zeroship/bootstrap

Framework-internal dev dispatch and module normalization, pending retirement.
Creator code must not import this package.

The Vite plugin still consumes module normalization, the dev entry, fetch
routing and dev auth. The remaining workflow helpers belong to the coordinated
workflow refactor. Production procedure invocation uses the native runtime.

Database startup is owned by the runtime and DB plugin. The plugin supplies its
SDK adapter module, prepares collection wrappers from the validated descriptor,
and finalizes startup policy declarations. This package supplies no database
installer, policy capability handle or readiness promise.

Build with `node --run build` before building the Vite plugin. The runtime core
has no build dependency on this package; the DB adapter embeds its own SDK.
