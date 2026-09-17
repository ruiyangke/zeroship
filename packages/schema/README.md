# `@zeroship/schema`

The shared schema builder. One lexicon, consumed by both sides:

- `@zeroship/db` depends on it and re-exports `t`, `TypeBuilder`, `FieldDef` and
  friends, so creator-facing imports are unchanged.
- `@zeroship/migrate` marks it `noExternal` and bundles it, so the published
  migration package keeps its zero-runtime-dependency promise.

It declares no dependencies, and it compiles without any ambient runtime types -
`"types": []` in its tsconfig, so a reference to `process` or a `node:*` module
fails the build. Both properties are load-bearing: the package must be importable
in plain Node with no runtime environment, which is where the migration toolchain
runs.

Design authority: [`docs/proposals/2026-09-16-shared-schema-builder.md`](../../docs/proposals/2026-09-16-shared-schema-builder.md).
