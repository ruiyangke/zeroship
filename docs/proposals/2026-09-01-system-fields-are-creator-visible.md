# System fields are creator-visible, and read-only where they must be

Status: proposal, 2026-09-01. Supersedes the hide-by-default posture in
`sdks/bootstrap/src/install-schema.ts`.

Operator directive, 2026-09-01: *"for the system fields, we should be
transparent, we can make some fields readonly from creator code, just like
salesforce, but we should expose these fields to creator, less restrictions."*

## The finding: the runtime already implements this, and the SDK undoes it

The two layers disagree, and the restrictive one is the SDK.

**The Rust runtime is already Salesforce-shaped.** Of the seven system fields,
`crates/zeroship-plugin-db/src/crud/system_fields_pass.rs:46` marks exactly
three immutable:

```rust
pub(crate) const IMMUTABLE_SYSTEM_FIELDS: &[&str] = &["id", "created_at", "created_by"];
```

An UPDATE naming one of those three is refused at `:425` (and again at `:395`
for the `$set`-nested form) with `ImmutableSystemField`. The other four are
deliberately writable: `version`, `updated_at` and `updated_by` set
`creator_supplied_*` hints that *suppress* the auto-bump, so a creator who
supplies them wins; `deleted_at` is owned by `delete()` / `restore()`. That is
the Salesforce split already, decided and enforced, with the audit fields
overridable the way Salesforce's "Set Audit Fields" permission allows.

**The SDK then hides all seven.** `install-schema.ts:308` refuses any creator
schema declaring one of `SYSTEM_FIELD_NAMES` (`:249`), with a single sanctioned
exception for `id: t.id("prefix")`. And `:1072` strips all seven out of the
descriptor-derived field list before it reaches `model()`.

**The strip is not a policy. It is a workaround for the refusal.** `model()`
(`:555`) passes its field record straight to `normalizeSchema()` (`:579`), and
`normalizeSchema` is where the refusal at `:308` lives. The platform's own
generated descriptor contains all seven system fields; without the strip, the
runtime's own descriptor would throw `RESERVED_SYSTEM_FIELD_NAME` on boot. So
`stripRuntimeSystemFields` exists to stop the refusal firing on the platform's
own data, and its side effect - system fields absent from the creator-visible
Collection - is what makes them invisible.

The two are one coupled mechanism. Narrowing the refusal removes the need for
the strip, and removing the strip is what exposes the fields.

## Two defects found while establishing the above

**1. The refusal is keyed on the declared field name, not the resolved column.**
`:308` tests `SYSTEM_FIELD_NAMES.includes(key)` against the name as authored.
The default naming strategy is `naming.asIs` (`:1008`), but `naming.snakeCase`
is a supported opt-in (`:936`). Under `snakeCase`, a creator declaring
`createdAt` is **not refused** - `createdAt` is not in the list - and it resolves
to column `created_at`, the system column. The check is simultaneously too
strict for snake_case authors and blind to the camelCase authors who actually
collide. Whatever policy replaces it must key on the **resolved column name**.

**2. Two system fields are already exposed, under different spellings.**
`model()` injects `deletedAt` at `:583` when soft-delete is on and `version` at
`:588` when versioning is on. `SYSTEM_FIELD_NAMES` is snake_case, so `deletedAt`
never collides with the `deleted_at` in the strip list. Creators can already see
and write two system fields today; the hiding is not even uniform.

## The design

**Expose all seven on the Collection.** Delete `stripRuntimeSystemFields`
(`:192`) and its call site (`:1072`). System fields appear in generated types,
in query results, in filters and in sorts, like every other column.

**Replace the declaration refusal with a write classification.** Three tiers,
named in one place and enforced on both sides of the V8 boundary:

| Class | Fields | Creator may |
| --- | --- | --- |
| `readonly` | `id`, `created_at`, `created_by` | read, filter, sort |
| `defaulted` | `updated_at`, `updated_by`, `version` | the above, and supply a value on write, which wins over the auto-bump |
| `managed` | `deleted_at` | the above, but only through `delete()` / `restore()` |

This is exactly the split `IMMUTABLE_SYSTEM_FIELDS` already enforces. The
proposal does not invent a policy; it makes the SDK stop contradicting the one
that ships.

**Keep `id: t.id("prefix")`.** It is a prefix declaration, not an override, and
it is already sanctioned at `:309-321`.

**Refuse only genuine collisions, on the resolved column name.** A creator
declaring a field that resolves to a system column with a *different type or
meaning* is an error; a creator declaring `id: t.id("post")` is not. The check
moves after naming-strategy resolution so defect 1 cannot recur.

**Read-only is enforced in Rust, not TypeScript.** The SDK class drives types
and a fast error message; the fence stays `system_fields_pass.rs`. Per the
privilege invariant, a check that only exists in code the worker executes is not
a boundary - the TS side is ergonomics, the Rust side is the rule.

## Why this is safe to widen

Exposure adds no write capability that does not already exist. The runtime
accepts `version` / `updated_at` / `updated_by` from creator code today and
refuses `id` / `created_at` / `created_by` today; neither changes. What changes
is that creators can **read** their own audit columns without a raw escape
hatch, and that the type surface stops lying about what the row contains.

The narrowing this removes was never a security boundary. It was a schema-
authoring fence enforced in the SDK, and `system_fields_pass.rs` is the fence
that actually holds.

## What this does not settle

Whether `insert()` should accept a caller-supplied `id`. The observation that it
is discarded is solid; the mechanism is still open (task #126), and this
proposal deliberately leaves `id` in the `readonly` class rather than resolving
that question by side effect. If a caller-supplied `id` is later accepted, it
becomes a write-once-on-INSERT field, which is a change to
`IMMUTABLE_SYSTEM_FIELDS`'s INSERT arm, not to this classification.

## Acceptance

1. A creator schema declaring `created_at: t.date()` is accepted, and reads
   return the platform's value.
2. An UPDATE patch naming `created_at` is refused by the runtime, with the
   error surfacing through the SDK.
3. An UPDATE patch naming `updated_at` is accepted and the supplied value wins
   over the auto-bump - the behaviour `creator_supplied_updated_at` already
   selects.
4. Under `naming.snakeCase`, a creator declaring `createdAt` is refused as a
   collision. This test fails on today's code (defect 1) and is the regression
   guard for it.
5. Generated types for a collection include all seven system fields, with the
   three `readonly` ones typed so an assignment is a compile error.
