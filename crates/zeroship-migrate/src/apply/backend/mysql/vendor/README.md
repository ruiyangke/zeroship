# mysql2 platform driver bundle

`mysql2-3.14.1.bundle.mjs` is the platform-owned driver asset for the Phase E
Trusted JS-driver isolate. It is generated from unmodified `mysql2@3.14.1`
using the package's promise entrypoint.

Regenerate with:

```bash
crates/zeroship-migrate/scripts/vendor-mysql2.sh
```

The bundle keeps `node:*` imports external so they bind to
`zeroship-runtime` native SyntheticModules (`node:net`, `node:tls`,
`node:buffer`, and friends). Bare ambient Node modules are shimmed by the regen
script only; the driver package source is not patched.
